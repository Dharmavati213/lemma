//! `lemma-fake-agent` on its own, driven with raw JSON-RPC lines (no ACP SDK): the checks it
//! makes of its client and the adapter shapes it mirrors.

mod acp_support;

use acp_support::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A fake agent process and the messages it writes.
struct Raw {
    child: Child,
    stdin: Option<ChildStdin>,
    out: mpsc::Receiver<(Instant, Value)>,
}

impl Raw {
    fn start(sb: &Sandbox, scenario: &Value) -> Self {
        let path = sb.scenario(scenario);
        let mut child = Command::new(&bins().fake_agent)
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, out) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line).expect("the agent writes JSON lines");
                if tx.send((Instant::now(), v)).is_err() {
                    break;
                }
            }
        });
        Self { child, stdin, out }
    }

    fn send(&mut self, msg: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Messages up to and including the first matching one.
    fn until(&self, pred: impl Fn(&Value) -> bool) -> Vec<(Instant, Value)> {
        let mut seen = Vec::new();
        loop {
            let msg = self
                .out
                .recv_timeout(SLOW)
                .unwrap_or_else(|_| panic!("no matching message; got {seen:#?}"));
            let done = pred(&msg.1);
            seen.push(msg);
            if done {
                return seen;
            }
        }
    }

    fn response(&self, id: u64) -> Value {
        self.until(|m| m["id"] == json!(id) && m.get("method").is_none())
            .pop()
            .unwrap()
            .1
    }

    fn initialize(&mut self, notices: bool) {
        let mut caps =
            json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false});
        if notices {
            caps["session"] = json!({"notices": {}});
        }
        self.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
                         "params": {"protocolVersion": 1, "clientCapabilities": caps}}));
        let r = self.response(0);
        assert_eq!(r["result"]["protocolVersion"], 1);
        assert_eq!(r["result"]["agentInfo"]["name"], "lemma-fake-agent");
        assert_eq!(r["result"]["authMethods"], json!([]));
    }

    fn new_session(&mut self, sb: &Sandbox) -> String {
        self.send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {
            "cwd": sb.project(), "mcpServers": [],
            "_meta": {"claudeCode": {"options": {"disallowedTools": ["Edit", "Write", "NotebookEdit"],
                                                  "allowedTools": ["mcp__lemma__*"]}},
                      "systemPrompt": {"append": "test"}}}}));
        let r = self.response(1);
        r["result"]["sessionId"].as_str().unwrap().to_owned()
    }

    fn close_stdin_and_wait(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the agent did not exit on stdin EOF"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn skip_mcp(steps: Value) -> Value {
    json!({"_options": {"mcp": "skip"}, "default": steps})
}

#[test]
fn it_exits_on_stdin_eof() {
    let sb = Sandbox::new("raw-eof");
    let mut raw = Raw::start(&sb, &skip_mcp(json!([])));
    raw.initialize(true);
    let status = raw.close_stdin_and_wait();
    assert!(status.success(), "{status:?}");
}

#[test]
fn session_new_requires_the_disallowed_edit_tools() {
    let sb = Sandbox::new("raw-meta");
    let mut raw = Raw::start(&sb, &skip_mcp(json!([])));
    raw.initialize(true);
    raw.send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {
        "cwd": sb.project(), "mcpServers": [],
        "_meta": {"claudeCode": {"options": {"disallowedTools": ["Edit"]}}}}}),
    );
    let r = raw.response(1);
    assert_eq!(r["error"]["code"], -32602);
    let message = r["error"]["message"].as_str().unwrap();
    assert!(message.contains("disallowedTools"), "{message}");
    assert!(message.contains("Write, NotebookEdit"), "{message}");
}

#[test]
fn session_new_needs_the_lemma_mcp_server_unless_skipped() {
    let sb = Sandbox::new("raw-mcp");
    let mut raw = Raw::start(&sb, &json!({"default": []}));
    raw.initialize(true);
    raw.new_session_expecting_error(&sb, "no MCP server named \"lemma\"");
}

impl Raw {
    fn new_session_expecting_error(&mut self, sb: &Sandbox, text: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {
            "cwd": sb.project(), "mcpServers": [],
            "_meta": {"claudeCode": {"options": {"disallowedTools": ["Edit", "Write", "NotebookEdit"]}}}}}));
        let r = self.response(1);
        let message = r["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(text), "{r}");
    }
}

#[test]
fn modes_and_config_options_mirror_the_adapter() {
    let sb = Sandbox::new("raw-modes");
    let mut raw = Raw::start(&sb, &skip_mcp(json!([])));
    raw.initialize(true);
    raw.send(json!({"jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {
        "cwd": sb.project(), "mcpServers": [],
        "_meta": {"claudeCode": {"options": {"disallowedTools": ["Edit", "Write", "NotebookEdit"]}}}}}));
    let msgs = raw.until(|m| m["params"]["update"]["sessionUpdate"] == "available_commands_update");
    let new = &msgs.iter().find(|(_, m)| m["id"] == 1).unwrap().1["result"];
    let sid = new["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(new["modes"]["currentModeId"], "default");
    let ids: Vec<&str> = new["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["mode", "model", "effort"]);
    let commands = &msgs.last().unwrap().1["params"]["update"]["availableCommands"];
    assert_eq!(commands[0]["name"], "compact");
    assert_eq!(commands[1]["name"], "init");

    // set_mode: a config_option_update, then {} (no current_mode_update), like the adapter.
    raw.send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/set_mode",
                    "params": {"sessionId": sid, "modeId": "plan"}}),
    );
    let msgs = raw.until(|m| m["id"] == 2);
    let kinds: Vec<Value> = msgs
        .iter()
        .map(|(_, m)| m["params"]["update"]["sessionUpdate"].clone())
        .collect();
    assert_eq!(kinds, [json!("config_option_update"), Value::Null]);
    assert_eq!(msgs[1].1["result"], json!({}));

    // set_config_option on mode: current_mode_update, then {configOptions}.
    raw.send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "session/set_config_option",
                    "params": {"sessionId": sid, "configId": "mode", "value": "default"}}),
    );
    let msgs = raw.until(|m| m["id"] == 3);
    assert_eq!(
        msgs[0].1["params"]["update"]["sessionUpdate"],
        "current_mode_update"
    );
    assert_eq!(
        msgs[1].1["result"]["configOptions"][0]["currentValue"],
        "default"
    );

    // An unknown mode is refused.
    raw.send(
        json!({"jsonrpc": "2.0", "id": 4, "method": "session/set_mode",
                    "params": {"sessionId": sid, "modeId": "bypassPermissions"}}),
    );
    assert_eq!(raw.response(4)["error"]["code"], -32602);
}

#[test]
fn notices_fall_back_to_transcript_text_without_the_capability() {
    let sb = Sandbox::new("raw-notice");
    let mut raw = Raw::start(
        &sb,
        &skip_mcp(
            json!([{"notice": {"title": "Fast mode turned off", "description": "not available."}}]),
        ),
    );
    raw.initialize(false);
    let sid = raw.new_session(&sb);
    raw.send(
        json!({"jsonrpc": "2.0", "id": 5, "method": "session/prompt",
                    "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]}}),
    );
    let msgs = raw.until(|m| m["id"] == 5);
    let update = msgs
        .iter()
        .find(|(_, m)| {
            m["method"] == "session/update"
                && m["params"]["update"]["sessionUpdate"] != "available_commands_update"
        })
        .map(|(_, m)| &m["params"]["update"])
        .unwrap();
    assert_eq!(
        update,
        &json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "**Fast mode turned off:** not available."}})
    );
    assert_eq!(msgs.last().unwrap().1["result"]["stopReason"], "end_turn");
}

#[test]
fn say_comes_in_three_pieces_20_ms_apart() {
    let sb = Sandbox::new("raw-say");
    let mut raw = Raw::start(&sb, &skip_mcp(json!([{"say": "abcdefghi"}])));
    raw.initialize(true);
    let sid = raw.new_session(&sb);
    raw.send(
        json!({"jsonrpc": "2.0", "id": 5, "method": "session/prompt",
                    "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]}}),
    );
    let msgs = raw.until(|m| m["id"] == 5);
    let pieces: Vec<(Instant, &str)> = msgs
        .iter()
        .filter(|(_, m)| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .map(|(t, m)| {
            (
                *t,
                m["params"]["update"]["content"]["text"].as_str().unwrap(),
            )
        })
        .collect();
    let texts: Vec<&str> = pieces.iter().map(|(_, t)| *t).collect();
    assert_eq!(texts, ["abc", "def", "ghi"]);
    for w in pieces.windows(2) {
        assert!(
            w[1].0 - w[0].0 >= Duration::from_millis(15),
            "pieces arrive about 20 ms apart"
        );
    }
}

#[test]
fn cancel_and_busy_prompts() {
    let sb = Sandbox::new("raw-cancel");
    let mut raw = Raw::start(&sb, &skip_mcp(json!([{"sleep_ms": 30000}])));
    raw.initialize(true);
    let sid = raw.new_session(&sb);
    raw.send(
        json!({"jsonrpc": "2.0", "id": 5, "method": "session/prompt",
                    "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]}}),
    );
    raw.send(
        json!({"jsonrpc": "2.0", "id": 6, "method": "session/prompt",
                    "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "again"}]}}),
    );
    assert_eq!(raw.response(6)["error"]["code"], -32600);
    let t0 = Instant::now();
    raw.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}));
    assert_eq!(raw.response(5)["result"]["stopReason"], "cancelled");
    assert!(t0.elapsed() < Duration::from_millis(500));
    // session/close answers and the session stays open for EOF.
    raw.send(
        json!({"jsonrpc": "2.0", "id": 7, "method": "session/close", "params": {"sessionId": sid}}),
    );
    assert_eq!(raw.response(7)["result"], json!({}));
    assert!(raw.close_stdin_and_wait().success());
}

#[test]
fn check_mode_validates_scenario_files() {
    let sb = Sandbox::new("raw-check");
    let fixtures_dir = fixtures().join("agent");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&fixtures_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    assert_eq!(files.len(), 7, "{files:?}");
    let out = Command::new(&bins().fake_agent)
        .arg("--check")
        .args(&files)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bad = sb.scenario(&json!({"default": [{"tool": {"name": "edit", "expect_error": 3}}]}));
    let out = Command::new(&bins().fake_agent)
        .arg("--check")
        .arg(&bad)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("\"default\" step 1") && err.contains("expect_error"),
        "{err}"
    );
}
