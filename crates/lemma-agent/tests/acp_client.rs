//! The ACP client (`lemma_agent::spawn`) against `lemma-fake-agent` (WP2.1 + WP2.3 acceptance).
//! Every test runs a real agent process, Lemma's MCP server on a unix socket and the real
//! `lemma-mcp` bridge between them, exactly as in the editor (only node + the adapter are
//! replaced by the fake agent).

mod acp_support;

use acp_support::*;
use lemma_agent::{
    AgentCmd, AgentEvent, ConfigOpt, EditorRequest, PermOption, PromptBlock, StopReason, ToolStatus,
};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn values(option: &ConfigOpt) -> Vec<&str> {
    option.values.iter().map(|(v, _)| v.as_str()).collect()
}

fn option<'a>(options: &'a [ConfigOpt], id: &str) -> &'a ConfigOpt {
    options
        .iter()
        .find(|o| o.id == id)
        .unwrap_or_else(|| panic!("no option {id} in {options:?}"))
}

#[test]
fn handshake_reports_ready_with_agent_modes_and_options() {
    let sb = Sandbox::new("ready");
    let scenario = sb.scenario(&json!({"default": [{"say": "hi"}]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    let AgentEvent::Ready {
        session,
        agent,
        modes,
        mode,
        options,
    } = expect_ready(&handle)
    else {
        unreachable!()
    };
    assert!(session.starts_with("fake-session-"), "{session}");
    assert_eq!(
        agent,
        format!("lemma-fake-agent {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        modes,
        [
            ("default".to_owned(), "Manual".to_owned()),
            ("plan".to_owned(), "Plan".to_owned())
        ]
    );
    assert_eq!(mode, "default");
    let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["mode", "model", "effort"]);
    assert_eq!(values(option(&options, "mode")), ["default", "plan"]);
    assert_eq!(
        values(option(&options, "model")),
        ["default", "opus", "sonnet", "haiku"]
    );
    assert_eq!(
        values(option(&options, "effort")),
        ["default", "low", "medium", "high", "xhigh", "max"]
    );
    assert_eq!(option(&options, "model").current, "default");
    // The adapter sends its slash commands right after session/new.
    match next_event(&handle, SLOW) {
        Some(AgentEvent::Commands(commands)) => {
            let names: Vec<&str> = commands.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, ["compact", "init"]);
        }
        other => panic!("expected Commands, got {other:?}"),
    }
    let rest = shutdown(&handle);
    assert_eq!(rest, [AgentEvent::Exited(Some(0))]);
    assert!(handle.cmd.send_blocking(AgentCmd::Cancel).is_err());
}

#[test]
fn a_prompt_streams_chunks_in_order_and_ends_the_turn() {
    let sb = Sandbox::new("stream");
    let scenario = sb.scenario(&json!({"default": [
        {"think": "Thinking about sheaves."},
        {"say": "Hello, here is the answer: $\\mathcal{O}_X$."},
        {"plan": ["one", {"content": "two", "status": "completed"}]},
        {"usage": {"used": 1234, "size": 200000}},
        {"notice": {"title": "Fast mode turned off", "description": "Not available.", "severity": "warning"}},
        {"say": "Done."}
    ]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    let interesting: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| !matches!(e, AgentEvent::Commands(_)))
        .collect();
    let thoughts: String = interesting
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ThoughtChunk(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(thoughts, "Thinking about sheaves.");
    assert_eq!(
        message_text(&events),
        "Hello, here is the answer: $\\mathcal{O}_X$.Done."
    );
    // say/think arrive in three pieces each.
    assert_eq!(
        interesting
            .iter()
            .filter(|e| matches!(e, AgentEvent::MessageChunk(_)))
            .count(),
        6
    );
    let kinds: Vec<&str> = interesting
        .iter()
        .map(|e| match e {
            AgentEvent::ThoughtChunk(_) => "thought",
            AgentEvent::MessageChunk(_) => "message",
            AgentEvent::Plan(_) => "plan",
            AgentEvent::Usage { .. } => "usage",
            AgentEvent::Notice(_) => "notice",
            AgentEvent::TurnEnded(_) => "end",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "thought", "thought", "thought", "message", "message", "message", "plan", "usage",
            "notice", "message", "message", "message", "end"
        ]
    );
    assert!(interesting.contains(&&AgentEvent::Plan(vec![
        ("one".into(), "pending".into()),
        ("two".into(), "completed".into())
    ])));
    assert!(interesting.contains(&&AgentEvent::Usage {
        used: 1234,
        size: 200000
    }));
    assert!(interesting.contains(&&AgentEvent::Notice(
        "Fast mode turned off: Not available.".into()
    )));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    assert!(errors(&events).is_empty());
    shutdown(&handle);
}

#[test]
fn a_thousand_chunks_arrive_without_loss_or_reordering() {
    let sb = Sandbox::new("chunks");
    let scenario = sb.scenario(&json!({"default": [
        {"stream": {"count": 1000, "text": "c{{i}};"}}
    ]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "stream");
    let events = events_until(&handle, SLOW, is_turn_end);
    let chunks: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageChunk(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let expected: Vec<String> = (1..=1000).map(|i| format!("c{i};")).collect();
    assert_eq!(chunks, expected);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    shutdown(&handle);
}

#[test]
fn tool_calls_go_through_the_mcp_server_and_report_back() {
    let sb = Sandbox::new("tools");
    sb.copy_fixture_project();
    let scenario = sb.scenario(&json!({"default": [
        {"tool": {"name": "read_file", "args": {"file_path": "main.tex", "limit": 3}, "expect_text": "documentclass"}},
        {"tool": {"name": "edit", "args": {"file_path": "main.tex", "old_string": "a", "new_string": "b"}}},
        {"tool": {"name": "edit", "args": {"file_path": "../outside.tex", "old_string": "a", "new_string": "b"},
                  "expect_error": "path outside project root"}},
        {"tool": {"name": "compile", "args": {}}}
    ]}));
    let mut handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    let tools = std::mem::replace(&mut handle.tools, async_channel::unbounded().1);
    let answerer = ToolAnswerer::start(tools, |req| match req {
        EditorRequest::ReadFile { .. } => {
            text_reply("     1\t\\documentclass[11pt]{amsart}", false)
        }
        EditorRequest::Edit { path, .. } if path.starts_with("..") => {
            text_reply("path outside project root", true)
        }
        EditorRequest::Edit { path, .. } => {
            text_reply(&format!("Edited {}: lines 1–1", path.display()), false)
        }
        EditorRequest::Compile => text_reply("Build succeeded: 5 pages, 1 warning", false),
        other => text_reply(&format!("unexpected {other:?}"), true),
    });
    expect_ready(&handle);
    prompt(&handle, "edit");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    // Each call: ToolCall (pending), ToolUpdate (in progress), ToolUpdate (done).
    let tool_events: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolCall { .. } | AgentEvent::ToolUpdate { .. }
            )
        })
        .collect();
    assert_eq!(tool_events.len(), 12, "{tool_events:#?}");
    let AgentEvent::ToolCall {
        id,
        title,
        kind,
        status,
    } = tool_events[3]
    else {
        panic!("{:?}", tool_events[3]);
    };
    assert_eq!(
        (title.as_str(), kind.as_str(), *status),
        ("mcp__lemma__edit", "other", ToolStatus::Pending)
    );
    assert_eq!(
        tool_events[4],
        &AgentEvent::ToolUpdate {
            id: id.clone(),
            title: None,
            status: Some(ToolStatus::InProgress)
        }
    );
    assert_eq!(
        tool_events[5],
        &AgentEvent::ToolUpdate {
            id: id.clone(),
            title: None,
            status: Some(ToolStatus::Completed)
        }
    );
    // The refused edit is reported as failed.
    assert!(matches!(
        tool_events[8],
        AgentEvent::ToolUpdate {
            status: Some(ToolStatus::Failed),
            ..
        }
    ));
    shutdown(&handle);
    // The MCP server stopped with the agent, which closes the tool channel.
    let calls = answerer.calls.clone();
    answerer.join();
    let calls = calls.lock().unwrap();
    let requests: Vec<&EditorRequest> = calls.iter().map(|(_, r)| r).collect();
    assert_eq!(
        requests,
        [
            &EditorRequest::ReadFile {
                path: PathBuf::from("main.tex"),
                offset: None,
                limit: Some(3)
            },
            &EditorRequest::Edit {
                path: PathBuf::from("main.tex"),
                old_string: "a".into(),
                new_string: "b".into(),
                replace_all: false
            },
            &EditorRequest::Edit {
                path: PathBuf::from("../outside.tex"),
                old_string: "a".into(),
                new_string: "b".into(),
                replace_all: false
            },
            &EditorRequest::Compile,
        ]
    );
    let ids: Vec<u64> = calls.iter().map(|(id, _)| *id).collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "call ids are unique: {ids:?}"
    );
}

#[test]
fn a_failed_expectation_is_an_error_and_ends_the_turn() {
    let sb = Sandbox::new("expect");
    let scenario = sb.scenario(&json!({"default": [
        {"tool": {"name": "edit", "args": {"file_path": "x.tex", "old_string": "a", "new_string": "b"}}},
        {"say": "not reached"}
    ]}));
    let mut handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    let tools = std::mem::replace(&mut handle.tools, async_channel::unbounded().1);
    let answerer = ToolAnswerer::start(tools, |_| {
        text_reply(
            "old_string not found — the user may have edited this region; call read_file",
            true,
        )
    });
    expect_ready(&handle);
    prompt(&handle, "edit");
    let events = events_until(&handle, SLOW, is_turn_end);
    let errs = errors(&events);
    assert_eq!(errs.len(), 1, "{events:#?}");
    assert!(
        errs[0].contains("expected success") && errs[0].contains("old_string not found"),
        "{}",
        errs[0]
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    assert_eq!(message_text(&events), "");
    // The session goes on: the next prompt works.
    prompt(&handle, "again");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert_eq!(errors(&events).len(), 1);
    shutdown(&handle);
    answerer.join();
}

fn permission_round_trip(keyword: &str, answer: &str) {
    let sb = Sandbox::new(&format!("perm-{keyword}"));
    let scenario = fixtures().join("agent/permission.json");
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, &format!("please {keyword}"));
    let events = events_until(&handle, SLOW, |e| {
        matches!(e, AgentEvent::Permission { .. })
    });
    let Some(AgentEvent::Permission {
        request,
        title,
        options,
    }) = events.last()
    else {
        unreachable!()
    };
    assert!(!title.is_empty());
    let expected_kinds: Vec<(&str, &str)> = options
        .iter()
        .map(|o| (o.id.as_str(), o.kind.as_str()))
        .collect();
    assert_eq!(
        expected_kinds,
        [
            ("allow-once", "allow_once"),
            ("allow-with-updates", "allow_always"),
            ("reject", "reject_once")
        ]
    );
    assert_eq!(options[0].name, "Yes");
    send(
        &handle,
        AgentCmd::Permission {
            request: *request,
            option_id: Some(answer.into()),
        },
    );
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert!(
        message_text(&events).contains(&format!("Permission answer: {answer}.")),
        "{events:#?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    shutdown(&handle);
}

#[test]
fn permission_allow_once_round_trip() {
    permission_round_trip("once", "allow-once");
}

#[test]
fn permission_allow_always_round_trip() {
    permission_round_trip("always", "allow-with-updates");
}

#[test]
fn permission_reject_round_trip() {
    permission_round_trip("reject", "reject");
}

#[test]
fn a_wrong_permission_answer_fails_the_fake_agents_expectation() {
    let sb = Sandbox::new("perm-wrong");
    let handle = lemma_agent::spawn(sb.config(&fixtures().join("agent/permission.json"))).unwrap();
    expect_ready(&handle);
    prompt(&handle, "once");
    let events = events_until(&handle, SLOW, |e| {
        matches!(e, AgentEvent::Permission { .. })
    });
    let Some(AgentEvent::Permission { request, .. }) = events.last() else {
        unreachable!()
    };
    send(
        &handle,
        AgentCmd::Permission {
            request: *request,
            option_id: Some("reject".into()),
        },
    );
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(
        errors(&events)
            .iter()
            .any(|e| e.contains("expected \"allow-once\"")),
        "{events:#?}"
    );
    shutdown(&handle);
}

#[test]
fn an_unoffered_option_id_is_refused_and_cancels_the_request() {
    let sb = Sandbox::new("perm-unknown");
    let handle = lemma_agent::spawn(sb.config(&fixtures().join("agent/permission.json"))).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go ahead");
    let events = events_until(&handle, SLOW, |e| {
        matches!(e, AgentEvent::Permission { .. })
    });
    let Some(AgentEvent::Permission { request, .. }) = events.last() else {
        unreachable!()
    };
    send(
        &handle,
        AgentCmd::Permission {
            request: *request,
            option_id: Some("exit-plan-bypass".into()),
        },
    );
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(
        errors(&events)
            .iter()
            .any(|e| e.contains("not one of the offered permission options")),
        "{events:#?}"
    );
    // The fake agent saw a Cancelled outcome and ended the turn as cancelled.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::Cancelled))
    );
    shutdown(&handle);
}

#[test]
fn exit_plan_options_never_offer_elevated_modes() {
    let sb = Sandbox::new("perm-plan");
    let handle = lemma_agent::spawn(sb.config(&fixtures().join("agent/permission.json"))).unwrap();
    expect_ready(&handle);
    prompt(&handle, "make a plan");
    let events = events_until(&handle, SLOW, |e| {
        matches!(e, AgentEvent::Permission { .. })
    });
    let Some(AgentEvent::Permission {
        request, options, ..
    }) = events.last()
    else {
        unreachable!()
    };
    assert_eq!(
        options,
        &[
            PermOption {
                id: "exit-plan-default".into(),
                name: "Yes, manually approve edits".into(),
                kind: "allow_once".into()
            },
            PermOption {
                id: "reject".into(),
                name: "No, keep planning".into(),
                kind: "reject_once".into()
            },
        ]
    );
    send(
        &handle,
        AgentCmd::Permission {
            request: *request,
            option_id: Some("exit-plan-default".into()),
        },
    );
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(errors(&events).is_empty(), "{events:#?}");
    shutdown(&handle);
}

#[test]
fn cancel_ends_a_running_turn_within_500_ms() {
    let sb = Sandbox::new("cancel");
    let handle = lemma_agent::spawn(sb.config(&fixtures().join("agent/cancel.json"))).unwrap();
    expect_ready(&handle);
    prompt(&handle, "start the long task");
    events_until(&handle, SLOW, |e| matches!(e, AgentEvent::MessageChunk(_)));
    let t0 = Instant::now();
    send(&handle, AgentCmd::Cancel);
    let events = events_until(&handle, SLOW, is_turn_end);
    let took = t0.elapsed();
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::Cancelled))
    );
    assert!(took < Duration::from_millis(500), "cancel took {took:?}");
    assert!(!message_text(&events).contains("only shown"));
    // The session is still usable.
    prompt(&handle, "start the long task");
    events_until(&handle, SLOW, |e| matches!(e, AgentEvent::MessageChunk(_)));
    send(&handle, AgentCmd::Cancel);
    let events = events_until(&handle, SLOW, is_turn_end);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::Cancelled))
    );
    shutdown(&handle);
}

#[test]
fn cancel_answers_pending_permissions_as_cancelled() {
    let sb = Sandbox::new("cancel-perm");
    let mut cfg = sb.config(&fixtures().join("agent/cancel.json"));
    let raw_log = sb.dir.join("raw.log");
    cfg.raw_log = Some(raw_log.clone());
    let handle = lemma_agent::spawn(cfg).unwrap();
    expect_ready(&handle);
    prompt(&handle, "permission first");
    events_until(&handle, SLOW, |e| {
        matches!(e, AgentEvent::Permission { .. })
    });
    let t0 = Instant::now();
    send(&handle, AgentCmd::Cancel);
    let events = events_until(&handle, SLOW, is_turn_end);
    let took = t0.elapsed();
    // The fake agent fails the turn (an Error) if the permission is not answered Cancelled.
    assert!(errors(&events).is_empty(), "{events:#?}");
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::Cancelled))
    );
    assert!(took < Duration::from_millis(500), "cancel took {took:?}");
    shutdown(&handle);
    let log = std::fs::read_to_string(&raw_log).unwrap();
    let answer = log
        .lines()
        .position(|l| l.starts_with(">> ") && l.contains(r#""outcome":{"outcome":"cancelled"}"#))
        .expect("permission answered cancelled");
    let cancel = log
        .lines()
        .position(|l| l.starts_with(">> ") && l.contains(r#""method":"session/cancel""#))
        .expect("session/cancel sent");
    assert!(
        answer < cancel,
        "the permission is answered before session/cancel"
    );
}

#[test]
fn a_prompt_while_a_turn_runs_is_refused() {
    let sb = Sandbox::new("busy");
    let handle = lemma_agent::spawn(sb.config(&fixtures().join("agent/cancel.json"))).unwrap();
    expect_ready(&handle);
    prompt(&handle, "first");
    events_until(&handle, SLOW, |e| matches!(e, AgentEvent::MessageChunk(_)));
    prompt(&handle, "second");
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::Error(_)));
    assert!(errors(&events)[0].contains("still working"));
    send(&handle, AgentCmd::Cancel);
    let events = events_until(&handle, SLOW, is_turn_end);
    assert_eq!(
        events.last(),
        Some(&AgentEvent::TurnEnded(StopReason::Cancelled))
    );
    // Exactly one TurnEnded for the accepted prompt.
    let rest = shutdown(&handle);
    assert!(!rest.iter().any(is_turn_end), "{rest:#?}");
}

#[test]
fn set_mode_and_set_config_are_confirmed() {
    let sb = Sandbox::new("config");
    let mut cfg = sb.config(&sb.scenario(&json!({"default": [{"say": "x"}]})));
    let raw_log = sb.dir.join("raw.log");
    cfg.raw_log = Some(raw_log.clone());
    let handle = lemma_agent::spawn(cfg).unwrap();
    expect_ready(&handle);

    send(&handle, AgentCmd::SetMode("plan".into()));
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::ModeChanged(_)));
    assert_eq!(events.last(), Some(&AgentEvent::ModeChanged("plan".into())));
    // The adapter answers set_mode with a config_option_update.
    let config = events.iter().rev().find_map(|e| match e {
        AgentEvent::ConfigChanged(o) => Some(o.clone()),
        _ => None,
    });
    assert_eq!(
        option(&config.expect("ConfigChanged"), "mode").current,
        "plan"
    );

    send(
        &handle,
        AgentCmd::SetConfig {
            id: "model".into(),
            value: "opus".into(),
        },
    );
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::ConfigChanged(_)));
    let Some(AgentEvent::ConfigChanged(options)) = events.last() else {
        unreachable!()
    };
    assert_eq!(option(options, "model").current, "opus");
    assert_eq!(option(options, "mode").current, "plan");

    send(
        &handle,
        AgentCmd::SetConfig {
            id: "effort".into(),
            value: "high".into(),
        },
    );
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::ConfigChanged(_)));
    let Some(AgentEvent::ConfigChanged(options)) = events.last() else {
        unreachable!()
    };
    assert_eq!(option(options, "effort").current, "high");

    // The mode config option goes through set_config_option too.
    send(
        &handle,
        AgentCmd::SetConfig {
            id: "mode".into(),
            value: "default".into(),
        },
    );
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::ModeChanged(_)));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::ModeChanged("default".into()))
    );

    // Modes Lemma does not offer never reach the agent.
    send(&handle, AgentCmd::SetMode("bypassPermissions".into()));
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::Error(_)));
    assert!(errors(&events)[0].contains("does not offer"));
    send(
        &handle,
        AgentCmd::SetConfig {
            id: "mode".into(),
            value: "auto".into(),
        },
    );
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::Error(_)));
    assert!(errors(&events)[0].contains("does not offer"));
    // An invalid value is the agent's error.
    send(
        &handle,
        AgentCmd::SetConfig {
            id: "model".into(),
            value: "gpt".into(),
        },
    );
    let events = events_until(&handle, SLOW, |e| matches!(e, AgentEvent::Error(_)));
    assert!(errors(&events)[0].contains("Invalid value"), "{events:?}");
    shutdown(&handle);
    let log = std::fs::read_to_string(&raw_log).unwrap();
    assert!(!log.contains("bypassPermissions"));
    assert!(!log.contains(r#""value":"auto""#));
}

#[test]
fn modes_lemma_does_not_offer_are_never_left_active() {
    let sb = Sandbox::new("modes");
    let scenario = sb.scenario(&json!({
        "_options": {"modes": "adapter", "initial_mode": "bypassPermissions"},
        "default": [{"mode": "auto"}, {"sleep_ms": 100}, {"say": "done"}]
    }));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    let AgentEvent::Ready {
        modes,
        mode,
        options,
        ..
    } = expect_ready(&handle)
    else {
        unreachable!()
    };
    // The session started in bypassPermissions; Lemma switched it to default before Ready.
    assert_eq!(mode, "default");
    let ids: Vec<&str> = modes.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["default", "plan"]);
    assert_eq!(values(option(&options, "mode")), ["default", "plan"]);
    assert_eq!(option(&options, "mode").current, "default");
    // The agent switches itself to auto: Lemma reports it and switches back.
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    let modes_seen: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModeChanged(m) => Some(m.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(modes_seen, ["auto", "default"], "{events:#?}");
    let notices = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice(n) if n.contains("does not offer")))
        .count();
    assert_eq!(notices, 1, "{events:#?}");
    shutdown(&handle);
}

#[test]
fn prompt_blocks_reach_the_agent_and_select_the_turn() {
    let sb = Sandbox::new("echo");
    let scenario = sb.scenario(&json!({
        "default": [{"say": "default turn"}],
        "echo": [{"echo_prompt": true}]
    }));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    send(
        &handle,
        AgentCmd::Prompt(vec![
            PromptBlock::Text("<review_feedback>none</review_feedback>".into()),
            PromptBlock::Text("Please ECHO this".into()),
            PromptBlock::Resource {
                uri: "file:///proj/main.tex#L10".into(),
                mime: "text/x-tex".into(),
                text: "\\section{Intro}".into(),
            },
        ]),
    );
    let events = events_until(&handle, SLOW, is_turn_end);
    let echoed: serde_json::Value = serde_json::from_str(&message_text(&events)).unwrap();
    assert_eq!(
        echoed,
        json!({"prompt": [
            {"type": "text", "text": "<review_feedback>none</review_feedback>"},
            {"type": "text", "text": "Please ECHO this"},
            {"type": "resource", "resource": {"uri": "file:///proj/main.tex#L10", "mimeType": "text/x-tex", "text": "\\section{Intro}"}}
        ]})
    );
    prompt(&handle, "something else");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert_eq!(message_text(&events), "default turn");
    shutdown(&handle);
}

#[test]
fn unknown_updates_are_ignored() {
    let sb = Sandbox::new("unknown");
    let scenario = sb.scenario(&json!({"default": [
        {"raw_update": {"sessionUpdate": "subagent_spawned", "agentId": "x"}},
        {"raw_update": {"sessionUpdate": "session_info_update", "title": "Sheaves"}},
        {"raw_update": {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "replayed"}}},
        {"say": "still here"}
    ]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert_eq!(message_text(&events), "still here");
    assert!(errors(&events).is_empty());
    shutdown(&handle);
}

#[test]
fn notices_arrive_as_notice_events() {
    // The client advertises notices, so the fake agent sends `notice` updates, not transcript text.
    let sb = Sandbox::new("notice");
    let scenario = sb.scenario(&json!({"default": [{"notice": "Context compacted"}]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(events.contains(&AgentEvent::Notice("Context compacted".into())));
    assert_eq!(message_text(&events), "");
    shutdown(&handle);
}

#[test]
fn stop_reasons_are_mapped() {
    let sb = Sandbox::new("stops");
    let scenario = sb.scenario(&json!({
        "default": [{"stop": "end_turn"}],
        "tokens": [{"stop": "max_tokens"}],
        "requests": [{"stop": "max_turn_requests"}],
        "refuse": [{"stop": "refusal"}],
        "cancelled": [{"stop": "cancelled"}]
    }));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    for (word, reason) in [
        ("hello", StopReason::EndTurn),
        ("tokens", StopReason::MaxTokens),
        ("requests", StopReason::MaxTurnRequests),
        ("refuse", StopReason::Refusal),
        ("cancelled", StopReason::Cancelled),
    ] {
        prompt(&handle, word);
        let events = events_until(&handle, SLOW, is_turn_end);
        assert_eq!(events.last(), Some(&AgentEvent::TurnEnded(reason)));
    }
    shutdown(&handle);
}

#[test]
fn death_during_startup_reports_the_stderr_tail() {
    let sb = Sandbox::new("startup-death");
    let scenario = sb.scenario(&json!({
        "_options": {"startup_stderr": "Error: Cannot find module '@anthropic-ai/claude-agent-sdk'",
                     "exit_on": "initialize", "exit_code": 3},
        "default": [{"say": "never"}]
    }));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    let events = drain(&handle, SLOW);
    assert_eq!(events.len(), 2, "{events:#?}");
    let AgentEvent::Error(message) = &events[0] else {
        panic!("{events:#?}")
    };
    assert!(message.contains("start-up"), "{message}");
    assert!(message.contains("exit code 3"), "{message}");
    assert!(
        message.contains("Cannot find module '@anthropic-ai/claude-agent-sdk'"),
        "{message}"
    );
    assert_eq!(events[1], AgentEvent::Exited(Some(3)));
    assert!(handle.cmd.send_blocking(AgentCmd::Cancel).is_err());
}

#[test]
fn a_session_error_at_startup_is_reported() {
    let sb = Sandbox::new("session-error");
    let scenario = sb.scenario(&json!({"default": [{"say": "never"}]}));
    let mut cfg = sb.config(&scenario);
    cfg.mcp_bridge = PathBuf::from("/nonexistent/lemma-mcp");
    let handle = lemma_agent::spawn(cfg).unwrap();
    let events = drain(&handle, SLOW);
    assert_eq!(events.len(), 2, "{events:#?}");
    let AgentEvent::Error(message) = &events[0] else {
        panic!("{events:#?}")
    };
    assert!(message.contains("could not start a session"), "{message}");
    assert!(message.contains("/nonexistent/lemma-mcp"), "{message}");
    // The agent exits by itself once its stdin closes.
    assert_eq!(events[1], AgentEvent::Exited(Some(0)));
}

#[test]
fn death_during_a_turn_ends_it_and_exits() {
    let sb = Sandbox::new("crash");
    let scenario = sb.scenario(&json!({"default": [
        {"say": "about to crash"},
        {"stderr": "panic: something broke"},
        {"exit": 7}
    ]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    let events = drain(&handle, SLOW);
    assert_eq!(message_text(&events), "about to crash");
    let tail: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| !matches!(e, AgentEvent::MessageChunk(_) | AgentEvent::Commands(_)))
        .collect();
    assert_eq!(
        tail,
        [
            &AgentEvent::TurnEnded(StopReason::EndTurn),
            &AgentEvent::Exited(Some(7))
        ]
    );
    assert!(handle.cmd.send_blocking(AgentCmd::Cancel).is_err());
}

#[test]
fn shutdown_during_a_turn_cancels_it() {
    let sb = Sandbox::new("shutdown-turn");
    let mut cfg = sb.config(&fixtures().join("agent/cancel.json"));
    let raw_log = sb.dir.join("raw.log");
    cfg.raw_log = Some(raw_log.clone());
    let handle = lemma_agent::spawn(cfg).unwrap();
    expect_ready(&handle);
    prompt(&handle, "permission please");
    events_until(&handle, SLOW, |e| {
        matches!(e, AgentEvent::Permission { .. })
    });
    let t0 = Instant::now();
    let rest = shutdown(&handle);
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert_eq!(
        rest,
        [
            AgentEvent::TurnEnded(StopReason::Cancelled),
            AgentEvent::Exited(Some(0))
        ]
    );
    let log = std::fs::read_to_string(&raw_log).unwrap();
    for needle in [
        r#""outcome":{"outcome":"cancelled"}"#,
        r#""method":"session/cancel""#,
        r#""method":"session/close""#,
    ] {
        assert!(
            log.lines()
                .any(|l| l.starts_with(">> ") && l.contains(needle)),
            "{needle} not sent"
        );
    }
}

#[test]
fn shutdown_during_startup_is_quick_and_quiet() {
    let sb = Sandbox::new("shutdown-start");
    let scenario = sb.scenario(&json!({
        "_options": {"session_new_delay_ms": 5000},
        "default": [{"say": "never"}]
    }));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    let events = shutdown(&handle);
    assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
    assert_eq!(events.len(), 1, "{events:#?}");
    assert!(matches!(events[0], AgentEvent::Exited(_)));
}

#[test]
fn dropping_the_command_sender_shuts_the_agent_down() {
    let sb = Sandbox::new("drop");
    let scenario = sb.scenario(&json!({"default": [{"say": "x"}]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    let lemma_agent::AgentHandle { cmd, events, tools } = handle;
    drop(cmd);
    let rest = drain(
        &lemma_agent::AgentHandle {
            cmd: async_channel::unbounded().0,
            events,
            tools,
        },
        SLOW,
    );
    assert_eq!(rest.last(), Some(&AgentEvent::Exited(Some(0))));
}

#[test]
fn raw_log_records_both_directions_privately() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new("rawlog");
    let mut cfg = sb.config(
        &sb.scenario(&json!({"default": [{"say": "logged"}, {"stderr": "a stderr line"}]})),
    );
    let raw_log = sb.dir.join("raw.log");
    cfg.raw_log = Some(raw_log.clone());
    let handle = lemma_agent::spawn(cfg).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    events_until(&handle, SLOW, is_turn_end);
    shutdown(&handle);
    let mode = std::fs::metadata(&raw_log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let log = std::fs::read_to_string(&raw_log).unwrap();
    let first = log.lines().next().unwrap();
    assert!(
        first.starts_with(">> ") && first.contains(r#""method":"initialize""#),
        "{first}"
    );
    assert!(
        log.lines()
            .any(|l| l.starts_with("<< ") && l.contains("agent_message_chunk"))
    );
    assert!(log.lines().any(|l| l == "!! a stderr line"));
    // §10.4 session/new contents.
    let new = log
        .lines()
        .find(|l| l.contains(r#""method":"session/new""#))
        .unwrap();
    let new: serde_json::Value = serde_json::from_str(&new[3..]).unwrap();
    let params = &new["params"];
    assert_eq!(params["cwd"], json!(sb.project()));
    assert_eq!(
        params["_meta"],
        json!({"claudeCode": {"options": {"disallowedTools": ["Edit", "Write", "NotebookEdit"],
                                          "allowedTools": ["mcp__lemma__*"]}},
               "systemPrompt": {"append": "You are working inside Lemma (test)."}})
    );
    assert_eq!(
        params["mcpServers"],
        json!([{"name": "lemma", "command": bins().mcp_bridge, "args": [],
                "env": [{"name": "LEMMA_MCP_SOCKET", "value": sb.socket()},
                        {"name": "LEMMA_MCP_TOKEN", "value": "0123456789abcdef0123456789abcdef"}]}])
    );
    let init: serde_json::Value = serde_json::from_str(&first[3..]).unwrap();
    assert_eq!(
        init["params"],
        json!({"protocolVersion": 1,
               "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false},
                                      "terminal": false, "session": {"notices": {}},
                                      // No terminal login: the adapter then offers no auth methods.
                                      "auth": {"terminal": false}},
               "clientInfo": {"name": "lemma", "title": "Lemma", "version": env!("CARGO_PKG_VERSION")}})
    );
}

#[test]
fn spawn_fails_for_a_missing_program_and_cleans_up() {
    let sb = Sandbox::new("missing");
    let mut cfg = sb.config(&sb.scenario(&json!({"default": []})));
    cfg.node = PathBuf::from("/nonexistent/node");
    let err = lemma_agent::spawn(cfg).err().expect("spawn fails");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(err.to_string().contains("/nonexistent/node"), "{err}");
    // The MCP server it started was stopped again.
    assert!(!sb.socket().exists());
}

#[test]
fn the_mcp_handshake_matches_claude_cli_and_a_callers_server_is_used() {
    let sb = Sandbox::new("handshake");
    let server = RecordingMcpServer::start(&sb.socket());
    let scenario = sb.scenario(&json!({"default": [
        {"tool": {"name": "outline", "args": {"file_path": "main.tex"}, "expect_text": "recorded"}}
    ]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(errors(&events).is_empty(), "{events:#?}");
    // The caller's server got the calls: the handle's tool channel is closed.
    assert!(handle.tools.is_closed());
    shutdown(&handle);
    let lines = server.lines();
    let expected = [
        r#"{"lemma_token":"0123456789abcdef0123456789abcdef"}"#,
        r#"{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.288","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"},"io.modelcontextprotocol/clientCapabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}}}}"#,
        r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.288","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"}},"jsonrpc":"2.0","id":0}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"method":"tools/list","jsonrpc":"2.0","id":1}"#,
        r#"{"method":"tools/call","params":{"name":"outline","arguments":{"file_path":"main.tex"}},"jsonrpc":"2.0","id":2}"#,
    ];
    assert_eq!(lines, expected);
}

#[test]
fn commands_sent_during_startup_run_after_ready() {
    let sb = Sandbox::new("queued");
    let scenario = sb.scenario(&json!({
        "_options": {"session_new_delay_ms": 300},
        "default": [{"say": "queued prompt ran"}]
    }));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    send(&handle, AgentCmd::SetMode("plan".into()));
    prompt(&handle, "hello");
    let events = events_until(&handle, SLOW, is_turn_end);
    assert!(
        matches!(&events[0], AgentEvent::Ready { mode, .. } if mode == "default"),
        "{events:#?}"
    );
    assert!(events.contains(&AgentEvent::ModeChanged("plan".into())));
    assert_eq!(message_text(&events), "queued prompt ran");
    assert!(errors(&events).is_empty(), "{events:#?}");
    shutdown(&handle);
}

#[test]
fn an_agent_that_closes_its_stdout_is_killed() {
    let sb = Sandbox::new("hung");
    let scenario = sb.scenario(&json!({"default": [
        {"say": "going quiet"}, {"close_stdout": true}
    ]}));
    let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    let t0 = Instant::now();
    let events = drain(&handle, SLOW);
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    let tail: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| !matches!(e, AgentEvent::MessageChunk(_) | AgentEvent::Commands(_)))
        .collect();
    // Still running when its stdout closed: Lemma killed it (SIGTERM), so no exit code.
    assert_eq!(
        tail,
        [
            &AgentEvent::TurnEnded(StopReason::EndTurn),
            &AgentEvent::Exited(None)
        ]
    );
}
