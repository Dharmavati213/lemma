//! The API-key backend (WP5.1) against a local fake Messages API: a std `TcpListener` serving
//! scripted SSE. Never the real API.

use lemma_agent::keys::ApiKey;
use lemma_agent::messages::{self, MessagesConfig};
use lemma_agent::{
    AgentCmd, AgentEvent, AgentHandle, EditorRequest, PromptBlock, StopReason, ToolContent,
    ToolReply, ToolStatus, Usd, mcp,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Never a real key. Every test uses it, so the log capture covers all of them.
const KEY: &str = "sk-ant-test-KEY-0f1e2d3c4b5a69788796a5b4c3d2e1f0";
const WAIT: Duration = Duration::from_secs(10);

// ============================================================================== log capture

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every tracing line of this test binary, at TRACE.
fn logs() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        let writer = capture.clone();
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .try_init();
        capture
    })
}

// ============================================================================== fake server

enum Step {
    Raw(String),
    Pause(Duration),
    /// Pings every 50 ms until a write fails (the client closed), for at most 10 s.
    PingUntilClosed,
}

enum Reply {
    Sse(Vec<Step>),
    Status {
        code: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    },
}

#[derive(Clone, Debug)]
struct Recorded {
    /// Lower-cased header names → values.
    headers: Vec<(String, String)>,
    body: Value,
    raw: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone)]
struct Fake {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    script: Arc<Mutex<VecDeque<Reply>>>,
    closed: Arc<AtomicBool>,
}

impl Fake {
    fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fake = Self {
            url: format!("http://{}", listener.local_addr().unwrap()),
            requests: Arc::default(),
            script: Arc::new(Mutex::new(replies.into())),
            closed: Arc::default(),
        };
        let server = fake.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let server = server.clone();
                thread::spawn(move || server.serve(stream));
            }
        });
        fake
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn wait_requests(&self, n: usize) -> Vec<Recorded> {
        let deadline = Instant::now() + WAIT;
        while self.requests.lock().unwrap().len() < n {
            assert!(Instant::now() < deadline, "expected {n} requests");
            thread::sleep(Duration::from_millis(5));
        }
        self.requests()
    }

    fn serve(&self, stream: TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.starts_with("POST /v1/messages "), "{line}");
        let mut headers = Vec::new();
        let mut length = 0;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            let (name, value) = l.split_once(':').unwrap();
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim().to_owned());
            if name == "content-length" {
                length = value.parse().unwrap();
            }
            headers.push((name, value));
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let raw = String::from_utf8(body).unwrap();
        let reply = self.script.lock().unwrap().pop_front();
        self.requests.lock().unwrap().push(Recorded {
            headers,
            body: serde_json::from_str(&raw).unwrap(),
            raw,
        });
        let mut out = stream;
        match reply.expect("a scripted reply for every request") {
            Reply::Status {
                code,
                headers,
                body,
            } => {
                let mut head = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n",
                    body.len()
                );
                for (n, v) in headers {
                    head.push_str(&format!("{n}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = out.write_all(head.as_bytes());
                let _ = out.write_all(body.as_bytes());
            }
            Reply::Sse(steps) => {
                let _ = out.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                );
                for step in steps {
                    match step {
                        Step::Raw(text) => {
                            let _ = out.write_all(text.as_bytes());
                            let _ = out.flush();
                        }
                        Step::Pause(d) => thread::sleep(d),
                        Step::PingUntilClosed => {
                            let deadline = Instant::now() + Duration::from_secs(10);
                            while Instant::now() < deadline {
                                let ping = sse("ping", &json!({"type": "ping"}));
                                if out.write_all(ping.as_bytes()).is_err() || out.flush().is_err() {
                                    self.closed.store(true, Ordering::SeqCst);
                                    return;
                                }
                                thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn sse(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// A scripted streamed response, built block by block.
struct Script {
    steps: Vec<Step>,
    index: u64,
}

impl Script {
    fn new(model: &str) -> Self {
        Self::with_usage(model, 10, 0, 0)
    }

    fn with_usage(model: &str, input: u64, cache_read: u64, cache_write: u64) -> Self {
        let start = json!({"type": "message_start", "message": {
            "id": "msg_1", "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": null, "usage": {"input_tokens": input, "output_tokens": 1,
            "cache_read_input_tokens": cache_read, "cache_creation_input_tokens": cache_write}}});
        Self {
            steps: vec![
                Step::Raw(sse("message_start", &start)),
                Step::Raw(sse("ping", &json!({"type": "ping"}))),
            ],
            index: 0,
        }
    }

    fn raw(mut self, text: String) -> Self {
        self.steps.push(Step::Raw(text));
        self
    }

    fn block(mut self, start: Value, deltas: &[Value]) -> Self {
        let i = self.index;
        self.index += 1;
        self.steps.push(Step::Raw(sse(
            "content_block_start",
            &json!({"type": "content_block_start", "index": i, "content_block": start}),
        )));
        for d in deltas {
            self.steps.push(Step::Raw(sse(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": i, "delta": d}),
            )));
        }
        self.steps.push(Step::Raw(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": i}),
        )));
        self
    }

    fn text(self, parts: &[&str]) -> Self {
        let deltas: Vec<Value> = parts
            .iter()
            .map(|t| json!({"type": "text_delta", "text": t}))
            .collect();
        self.block(json!({"type": "text", "text": ""}), &deltas)
    }

    fn thinking(self, parts: &[&str], signature: &str) -> Self {
        let mut deltas: Vec<Value> = parts
            .iter()
            .map(|t| json!({"type": "thinking_delta", "thinking": t}))
            .collect();
        deltas.push(json!({"type": "signature_delta", "signature": signature}));
        self.block(
            json!({"type": "thinking", "thinking": "", "signature": ""}),
            &deltas,
        )
    }

    fn tool(self, id: &str, name: &str, parts: &[&str]) -> Self {
        let deltas: Vec<Value> = parts
            .iter()
            .map(|p| json!({"type": "input_json_delta", "partial_json": p}))
            .collect();
        self.block(
            json!({"type": "tool_use", "id": id, "name": format!("mcp__lemma__{name}"), "input": {}}),
            &deltas,
        )
    }

    fn end(self, stop_reason: &str, output: u64) -> Reply {
        self.end_with(stop_reason, output, &Value::Null)
    }

    fn end_with(mut self, stop_reason: &str, output: u64, details: &Value) -> Reply {
        self.steps.push(Step::Raw(sse(
            "message_delta",
            &json!({"type": "message_delta", "delta": {"stop_reason": stop_reason,
                "stop_sequence": null, "stop_details": details}, "usage": {"output_tokens": output}}),
        )));
        self.steps.push(Step::Raw(sse(
            "message_stop",
            &json!({"type": "message_stop"}),
        )));
        Reply::Sse(self.steps)
    }

    fn then(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }
}

const OPUS: &str = "claude-opus-5-5";

// ============================================================================== the agent side

struct Session {
    handle: AgentHandle,
    events: Vec<AgentEvent>,
    served: Arc<Mutex<Vec<EditorRequest>>>,
}

type Answer = Arc<dyn Fn(&EditorRequest) -> ToolReply + Send + Sync>;

fn config(fake: &Fake) -> MessagesConfig {
    logs();
    let mut cfg = MessagesConfig::new(ApiKey::new(KEY).unwrap());
    cfg.base_url = Some(fake.url.clone());
    cfg.retry_base = Duration::from_millis(10);
    cfg
}

fn text(t: &str) -> ToolReply {
    ToolReply {
        content: vec![ToolContent::Text(t.into())],
        is_error: false,
    }
}

impl Session {
    fn start(cfg: MessagesConfig, answer: Answer) -> Self {
        let handle = messages::spawn(cfg).unwrap();
        let served: Arc<Mutex<Vec<EditorRequest>>> = Arc::default();
        let tools = handle.tools.clone();
        let log = Arc::clone(&served);
        thread::spawn(move || {
            while let Ok(call) = tools.recv_blocking() {
                log.lock().unwrap().push(call.req.clone());
                let _ = call.reply.send(answer(&call.req));
            }
        });
        let mut s = Self {
            handle,
            events: Vec::new(),
            served,
        };
        let ready = s.until(|e| matches!(e, AgentEvent::Ready { .. }));
        assert!(matches!(ready, AgentEvent::Ready { .. }));
        s
    }

    fn simple(fake: &Fake) -> Self {
        Self::start(config(fake), Arc::new(|_| text("ok")))
    }

    fn send(&self, cmd: AgentCmd) {
        self.handle.cmd.send_blocking(cmd).unwrap();
    }

    fn prompt(&self, text: &str) {
        self.send(AgentCmd::Prompt(vec![PromptBlock::Text(text.into())]));
    }

    /// Events until (and including) the first that matches; all are kept in `events`.
    fn until(&mut self, pred: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.handle.events.try_recv() {
                Ok(e) => {
                    self.events.push(e.clone());
                    if pred(&e) {
                        return e;
                    }
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("events closed: {:?}", self.events)
                }
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "timed out: {:#?}", self.events);
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    fn turn(&mut self) -> StopReason {
        match self.until(|e| matches!(e, AgentEvent::TurnEnded(_))) {
            AgentEvent::TurnEnded(r) => r,
            _ => unreachable!(),
        }
    }

    fn served(&self) -> Vec<EditorRequest> {
        self.served.lock().unwrap().clone()
    }

    fn shutdown(mut self) -> Vec<AgentEvent> {
        self.send(AgentCmd::Shutdown);
        self.until(|e| matches!(e, AgentEvent::Exited(_)));
        assert!(self.handle.cmd.send_blocking(AgentCmd::Cancel).is_err());
        self.events
    }
}

fn notices(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// `next`'s messages start with all of `prev`'s, unchanged (append-only history).
fn assert_append_only(prev: &Recorded, next: &Recorded) {
    let a = prev.body["messages"].as_array().unwrap();
    let b = next.body["messages"].as_array().unwrap();
    assert!(b.len() > a.len());
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.to_string(), y.to_string(), "an earlier message changed");
    }
    // Tools and system are byte-stable.
    assert_eq!(
        prev.body["tools"].to_string(),
        next.body["tools"].to_string()
    );
    assert_eq!(prev.body["system"], next.body["system"]);
}

// ============================================================================== tests

#[test]
fn streams_thinking_and_text_with_the_documented_request() {
    let fake = Fake::start(vec![
        Script::with_usage(OPUS, 1000, 2000, 3000)
            .thinking(&["Let me ", "think."], "sig-AbC/+=")
            .text(&["Hel", "lo"])
            .end("end_turn", 500),
        Script::new(OPUS).text(&["Again"]).end("end_turn", 5),
    ]);
    let mut s = Session::simple(&fake);
    match &s.events[0] {
        AgentEvent::Ready {
            modes,
            options,
            agent,
            ..
        } => {
            assert!(modes.is_empty());
            assert_eq!(agent, messages::AGENT_NAME);
            let ids: Vec<&str> = options.iter().map(|o| o.id.as_str()).collect();
            assert_eq!(ids, ["model", "effort"]);
            assert_eq!(options[0].current, "claude-opus-5-5");
            let models: Vec<&str> = options[0].values.iter().map(|(v, _)| v.as_str()).collect();
            assert_eq!(
                models,
                ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-4-5"]
            );
            assert_eq!(options[1].current, "high");
        }
        other => panic!("{other:?}"),
    }
    s.prompt("Hi");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let streamed: Vec<&AgentEvent> = s
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ThoughtChunk(_)
                    | AgentEvent::MessageChunk(_)
                    | AgentEvent::Usage { .. }
                    | AgentEvent::Cost { .. }
            )
        })
        .collect();
    assert_eq!(
        streamed,
        [
            &AgentEvent::ThoughtChunk("Let me ".into()),
            &AgentEvent::ThoughtChunk("think.".into()),
            &AgentEvent::MessageChunk("Hel".into()),
            &AgentEvent::MessageChunk("lo".into()),
            &AgentEvent::Usage {
                used: 6000,
                size: 1_000_000
            },
            // 1000 × $4 + 500 × $20 + 2000 × $0.20 + 3000 × $5, per million tokens.
            &AgentEvent::Cost {
                input: 1000,
                output: 500,
                cache_read: 2000,
                cache_write: 3000,
                usd_estimate: Usd(29_400),
            },
        ]
    );

    let r = &fake.requests()[0];
    assert_eq!(r.header("x-api-key"), Some(KEY));
    assert_eq!(r.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(
        r.header("anthropic-beta"),
        Some("server-side-fallback-2026-07-01")
    );
    let b = &r.body;
    assert_eq!(b["model"], "claude-opus-5-5");
    assert_eq!(b["max_tokens"], 64_000);
    assert_eq!(b["stream"], true);
    assert_eq!(
        b["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(b["output_config"], json!({"effort": "high"}));
    assert_eq!(b["fallbacks"], "default");
    assert_eq!(b["tool_choice"], json!({"type": "auto"}));
    assert_eq!(b["system"], messages::SYSTEM_PROMPT);
    assert_eq!(b["tools"], messages::tool_schemas());
    assert_eq!(b["tools"].as_array().unwrap().len(), mcp::TOOL_NAMES.len());
    // Cache control: once, at the top level.
    assert_eq!(b["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(r.raw.matches("cache_control").count(), 1);
    assert!(r.raw.starts_with("{\"model\":"));
    assert_eq!(
        b["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "Hi"}]}])
    );

    s.prompt("More");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let rs = fake.wait_requests(2);
    assert_append_only(&rs[0], &rs[1]);
    assert_eq!(
        rs[1].body["messages"][1],
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "Let me think.", "signature": "sig-AbC/+="},
            {"type": "text", "text": "Hello"}
        ]})
    );
    // The thinking block is echoed byte for byte.
    assert!(
        rs[1]
            .raw
            .contains(r#"{"signature":"sig-AbC/+=","thinking":"Let me think.","type":"thinking"}"#)
            || rs[1].raw.contains(
                r#"{"type":"thinking","thinking":"Let me think.","signature":"sig-AbC/+="}"#
            )
    );
    assert_eq!(rs[1].body["messages"][2]["content"][0]["text"], "More");
    let events = s.shutdown();
    assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(0))));
}

#[test]
fn a_tool_round_trip_runs_parallel_calls_in_order_and_answers_in_one_message() {
    let fake = Fake::start(vec![
        Script::new(OPUS)
            .text(&["Reading."])
            .tool(
                "toolu_a",
                "read_file",
                &["{\"file_", "path\": \"main.tex\"", "}"],
            )
            .tool("toolu_b", "outline", &[])
            .end("tool_use", 30),
        Script::new(OPUS).text(&["Done."]).end("end_turn", 3),
    ]);
    let answer: Answer = Arc::new(|req| match req {
        EditorRequest::ReadFile { .. } => text("1\t\\documentclass{article}"),
        _ => text("1 Introduction"),
    });
    let mut s = Session::start(config(&fake), answer);
    s.prompt("Look");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert_eq!(
        s.served(),
        [
            EditorRequest::ReadFile {
                path: "main.tex".into(),
                offset: None,
                limit: None
            },
            EditorRequest::Outline { path: None },
        ]
    );
    let rs = fake.wait_requests(2);
    assert_append_only(&rs[0], &rs[1]);
    let msgs = rs[1].body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(
        msgs[1]["content"],
        json!([
            {"type": "text", "text": "Reading."},
            {"type": "tool_use", "id": "toolu_a", "name": "mcp__lemma__read_file",
             "input": {"file_path": "main.tex"}},
            {"type": "tool_use", "id": "toolu_b", "name": "mcp__lemma__outline", "input": {}},
        ])
    );
    assert_eq!(
        msgs[2],
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_a", "is_error": false,
             "content": [{"type": "text", "text": "1\t\\documentclass{article}"}]},
            {"type": "tool_result", "tool_use_id": "toolu_b", "is_error": false,
             "content": [{"type": "text", "text": "1 Introduction"}]},
        ]})
    );
    // Tool cards: pending while streamed, then running, then done, under the MCP names.
    let cards: Vec<&AgentEvent> = s
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::ToolCall { .. } | AgentEvent::ToolUpdate { .. }
            )
        })
        .collect();
    assert_eq!(
        cards,
        [
            &AgentEvent::ToolCall {
                id: "toolu_a".into(),
                title: "mcp__lemma__read_file".into(),
                kind: "other".into(),
                status: ToolStatus::Pending
            },
            &AgentEvent::ToolCall {
                id: "toolu_b".into(),
                title: "mcp__lemma__outline".into(),
                kind: "other".into(),
                status: ToolStatus::Pending
            },
            &AgentEvent::ToolUpdate {
                id: "toolu_a".into(),
                title: None,
                status: Some(ToolStatus::InProgress)
            },
            &AgentEvent::ToolUpdate {
                id: "toolu_a".into(),
                title: None,
                status: Some(ToolStatus::Completed)
            },
            &AgentEvent::ToolUpdate {
                id: "toolu_b".into(),
                title: None,
                status: Some(ToolStatus::InProgress)
            },
            &AgentEvent::ToolUpdate {
                id: "toolu_b".into(),
                title: None,
                status: Some(ToolStatus::Completed)
            },
        ]
    );
    s.shutdown();
}

#[test]
fn malformed_tool_json_is_answered_with_invalid_json_and_not_run() {
    let fake = Fake::start(vec![
        Script::new(OPUS)
            .tool(
                "toolu_bad",
                "edit",
                &["{\"file_path\": \"main.tex\", \"old_string\": \"a\"\"b"],
            )
            .tool("toolu_schema", "read_file", &["{\"path\": \"main.tex\"}"])
            .tool("toolu_ok", "get_selection", &["{}"])
            .end("tool_use", 30),
        Script::new(OPUS).text(&["Sorry."]).end("end_turn", 3),
    ]);
    let mut s = Session::simple(&fake);
    s.prompt("Edit");
    assert_eq!(s.turn(), StopReason::EndTurn);
    // Only the valid call reached the editor.
    assert_eq!(s.served(), [EditorRequest::Selection]);
    let rs = fake.wait_requests(2);
    let msgs = rs[1].body["messages"].as_array().unwrap();
    // The unparseable input is echoed as {}.
    assert_eq!(msgs[1]["content"][0]["input"], json!({}));
    let results = msgs[2]["content"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["tool_use_id"], "toolu_bad");
    assert_eq!(results[0]["is_error"], true);
    let bad: Value =
        serde_json::from_str(results[0]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        bad,
        json!({"INVALID_JSON": "{\"file_path\": \"main.tex\", \"old_string\": \"a\"\"b"})
    );
    assert_eq!(results[1]["is_error"], true);
    let schema: Value =
        serde_json::from_str(results[1]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(schema["INVALID_JSON"], "{\"path\":\"main.tex\"}");
    assert!(
        schema["error"]
            .as_str()
            .unwrap()
            .contains("unexpected argument")
    );
    assert_eq!(results[2]["is_error"], false);
    assert!(s.events.contains(&AgentEvent::ToolUpdate {
        id: "toolu_bad".into(),
        title: None,
        status: Some(ToolStatus::Failed)
    }));
    s.shutdown();
}

#[test]
fn max_tokens_and_refusal_stops_run_no_tools() {
    let fake = Fake::start(vec![
        Script::new(OPUS)
            .text(&["Writing"])
            .tool(
                "toolu_cut",
                "write",
                &["{\"file_path\": \"a.tex\", \"content\": \"x\"}"],
            )
            .end("max_tokens", 64_000),
        Script::new(OPUS)
            .text(&["I'll help with th"])
            .tool("toolu_ref", "compile", &["{}"])
            .end_with(
                "refusal",
                9,
                &json!({"type": "refusal", "category": "cyber",
                        "explanation": "This looks like exploit development."}),
            ),
        Script::new(OPUS).text(&["Fine."]).end("end_turn", 2),
    ]);
    let mut s = Session::simple(&fake);
    s.prompt("Write a lot");
    assert_eq!(s.turn(), StopReason::MaxTokens);
    s.prompt("Do something else");
    assert_eq!(s.turn(), StopReason::Refusal);
    assert!(s.served().is_empty(), "no tool may run: {:?}", s.served());
    let n = notices(&s.events);
    assert!(
        n.iter()
            .any(|t| t.contains("cyber") && t.contains("exploit development")),
        "{n:?}"
    );
    assert!(s.events.contains(&AgentEvent::ToolUpdate {
        id: "toolu_cut".into(),
        title: None,
        status: Some(ToolStatus::Failed)
    }));
    assert!(s.events.contains(&AgentEvent::ToolUpdate {
        id: "toolu_ref".into(),
        title: None,
        status: Some(ToolStatus::Failed)
    }));
    s.prompt("Third");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let rs = fake.wait_requests(3);
    // After max_tokens: the cut tool use is echoed and answered "not run".
    let msgs = rs[1].body["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["content"][1]["id"], "toolu_cut");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "toolu_cut");
    assert_eq!(msgs[2]["content"][0]["is_error"], true);
    assert_eq!(msgs[3]["content"][0]["text"], "Do something else");
    // After the refusal: neither the refused response nor its prompt.
    assert_append_only(&rs[0], &rs[2]);
    let msgs = rs[2].body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[3]["content"][0]["text"], "Third");
    assert!(!rs[2].raw.contains("I'll help with th"));
    s.shutdown();
}

#[test]
fn retries_429_and_529_honouring_retry_after_but_not_401() {
    let err = |kind: &str, msg: &str| {
        json!({"type": "error", "error": {"type": kind, "message": msg}}).to_string()
    };
    let fake = Fake::start(vec![
        Reply::Status {
            code: 429,
            headers: vec![("retry-after", "1".into())],
            body: err("rate_limit_error", "slow down"),
        },
        Reply::Status {
            code: 529,
            headers: vec![("retry-after-ms", "200".into())],
            body: err("overloaded_error", "Overloaded"),
        },
        Script::new(OPUS).text(&["Finally"]).end("end_turn", 2),
        Reply::Status {
            code: 401,
            headers: vec![],
            body: err("authentication_error", "invalid x-api-key"),
        },
    ]);
    let mut s = Session::simple(&fake);
    let t0 = Instant::now();
    s.prompt("Hi");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let elapsed = t0.elapsed();
    assert!(elapsed >= Duration::from_millis(1190), "{elapsed:?}");
    assert_eq!(fake.requests().len(), 3);
    let n = notices(&s.events);
    assert_eq!(n.len(), 2, "{n:?}");
    assert!(
        n[0].contains("429") && n[0].contains("1.0 s") && n[0].contains("(1/3)"),
        "{n:?}"
    );
    assert!(
        n[1].contains("529") && n[1].contains("0.2 s") && n[1].contains("(2/3)"),
        "{n:?}"
    );
    assert!(
        s.events
            .contains(&AgentEvent::MessageChunk("Finally".into()))
    );

    s.prompt("Again");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let error = s
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Error(m) => Some(m.clone()),
            _ => None,
        })
        .unwrap();
    assert!(error.starts_with("API key rejected (401)"), "{error}");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(fake.requests().len(), 4, "401 is not retried");
    s.shutdown();
}

#[test]
fn gives_up_after_three_retries() {
    let five = || Reply::Status {
        code: 500,
        headers: vec![],
        body: "{}".into(),
    };
    let fake = Fake::start(vec![five(), five(), five(), five()]);
    let mut s = Session::simple(&fake);
    s.prompt("Hi");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert_eq!(fake.requests().len(), 4);
    assert!(
        s.events.iter().any(
            |e| matches!(e, AgentEvent::Error(m) if m.contains("500") && m.contains("gave up"))
        )
    );
    assert_eq!(notices(&s.events).len(), 3);
    s.shutdown();
}

#[test]
fn cancel_mid_stream_drops_the_connection_and_ends_the_turn() {
    let fake = Fake::start(vec![
        Script::new(OPUS)
            .text(&["partial"])
            .then(Step::PingUntilClosed)
            .end("end_turn", 1),
        Script::new(OPUS).text(&["Next"]).end("end_turn", 1),
    ]);
    let mut s = Session::simple(&fake);
    s.prompt("Long answer please");
    s.until(|e| *e == AgentEvent::MessageChunk("partial".into()));
    let t0 = Instant::now();
    s.send(AgentCmd::Cancel);
    assert_eq!(s.turn(), StopReason::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(1));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fake.closed.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the connection was not dropped");
        thread::sleep(Duration::from_millis(10));
    }
    s.prompt("Short then");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let rs = fake.wait_requests(2);
    // The cut-off response is not in the history.
    assert_append_only(&rs[0], &rs[1]);
    let msgs = rs[1].body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["role"], "user");
    assert!(!rs[1].raw.contains("partial"));
    s.shutdown();
}

#[test]
fn cancel_while_a_tool_runs_answers_every_tool_use() {
    let fake = Fake::start(vec![
        Script::new(OPUS)
            .tool("toolu_slow", "compile", &["{}"])
            .tool("toolu_next", "get_selection", &["{}"])
            .end("tool_use", 5),
        Script::new(OPUS).text(&["ok"]).end("end_turn", 1),
    ]);
    // The editor takes its time with the compile.
    let answer: Answer = Arc::new(|req| {
        if *req == EditorRequest::Compile {
            thread::sleep(Duration::from_secs(2));
        }
        text("built")
    });
    let mut s = Session::start(config(&fake), answer);
    s.prompt("Build");
    s.until(|e| {
        *e == AgentEvent::ToolUpdate {
            id: "toolu_slow".into(),
            title: None,
            status: Some(ToolStatus::InProgress),
        }
    });
    s.send(AgentCmd::Cancel);
    assert_eq!(s.turn(), StopReason::Cancelled);
    s.prompt("Again");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let rs = fake.wait_requests(2);
    let msgs = rs[1].body["messages"].as_array().unwrap();
    let results = msgs[2]["content"].as_array().unwrap();
    let ids: Vec<&str> = results
        .iter()
        .map(|r| r["tool_use_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["toolu_slow", "toolu_next"]);
    assert!(results.iter().all(|r| r["is_error"] == true));
    assert_eq!(msgs[3]["content"][0]["text"], "Again");
    s.shutdown();
}

#[test]
fn render_page_images_become_base64_image_blocks() {
    let png = b"\x89PNG\r\n\x1a\nfake".to_vec();
    let fake = Fake::start(vec![
        Script::new(OPUS)
            .tool("toolu_img", "render_page", &["{\"page\": 1}"])
            .end("tool_use", 5),
        Script::new(OPUS).text(&["I see it."]).end("end_turn", 1),
    ]);
    let image = png.clone();
    let answer: Answer = Arc::new(move |_| ToolReply {
        content: vec![
            ToolContent::Png(image.clone()),
            ToolContent::Text("Page 1 of 3".into()),
        ],
        is_error: false,
    });
    let mut s = Session::start(config(&fake), answer);
    s.prompt("Look at page 1");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert_eq!(s.served(), [EditorRequest::RenderPage { page: 1 }]);
    let rs = fake.wait_requests(2);
    assert_eq!(
        rs[1].body["messages"][2]["content"][0]["content"],
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                         "data": mcp::base64(&png)}},
            {"type": "text", "text": "Page 1 of 3"},
        ])
    );
    s.shutdown();
}

#[test]
fn haiku_gets_no_thinking_no_effort_and_a_200k_window() {
    let fake = Fake::start(vec![
        Script::with_usage("claude-haiku-4-5", 100, 0, 0)
            .text(&["Quick."])
            .end("end_turn", 10),
        // Served by another model than asked for.
        Script::new("claude-opus-5")
            .text(&["Fallback."])
            .end("end_turn", 1),
    ]);
    let mut s = Session::simple(&fake);
    s.send(AgentCmd::SetConfig {
        id: "model".into(),
        value: "claude-haiku-4-5".into(),
    });
    let changed = s.until(|e| matches!(e, AgentEvent::ConfigChanged(_)));
    let AgentEvent::ConfigChanged(options) = changed else {
        unreachable!()
    };
    assert_eq!(options.len(), 1, "no effort for Haiku: {options:?}");
    assert_eq!(options[0].current, "claude-haiku-4-5");
    s.send(AgentCmd::SetConfig {
        id: "effort".into(),
        value: "max".into(),
    });
    s.until(|e| matches!(e, AgentEvent::Error(_)));
    s.prompt("Hi");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert!(s.events.contains(&AgentEvent::Usage {
        used: 100,
        size: 200_000
    }));
    // 100 × $1 + 10 × $5 per million tokens.
    assert!(s.events.iter().any(|e| matches!(
        e,
        AgentEvent::Cost { usd_estimate, .. } if *usd_estimate == Usd(150)
    )));
    let r = &fake.requests()[0];
    assert_eq!(r.body["model"], "claude-haiku-4-5");
    for absent in ["thinking", "output_config", "fallbacks"] {
        assert!(r.body.get(absent).is_none(), "{absent}");
    }
    assert_eq!(r.header("anthropic-beta"), None);

    s.send(AgentCmd::SetConfig {
        id: "model".into(),
        value: "claude-sonnet-5-5".into(),
    });
    s.until(|e| matches!(e, AgentEvent::ConfigChanged(o) if o.len() == 2));
    s.prompt("Again");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert!(
        notices(&s.events)
            .iter()
            .any(|n| n.contains("claude-opus-5") && n.contains("claude-sonnet-5-5"))
    );
    let r = &fake.wait_requests(2)[1];
    assert_eq!(r.body["output_config"], json!({"effort": "high"}));
    s.shutdown();
}

#[test]
fn a_prompt_while_a_turn_runs_is_refused() {
    let fake = Fake::start(vec![
        Script::new(OPUS)
            .text(&["one"])
            .then(Step::Pause(Duration::from_millis(300)))
            .end("end_turn", 1),
    ]);
    let mut s = Session::simple(&fake);
    s.prompt("First");
    s.until(|e| *e == AgentEvent::MessageChunk("one".into()));
    s.prompt("Second");
    s.until(|e| matches!(e, AgentEvent::Error(m) if m.contains("already running")));
    assert_eq!(s.turn(), StopReason::EndTurn);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(fake.requests().len(), 1);
    let events = s.shutdown();
    let turns = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnEnded(_)))
        .count();
    assert_eq!(turns, 1);
}

#[test]
fn dropping_the_command_sender_shuts_down() {
    let fake = Fake::start(vec![]);
    let s = Session::simple(&fake);
    let AgentHandle { cmd, events, .. } = s.handle;
    drop(cmd);
    let deadline = Instant::now() + WAIT;
    let mut last = None;
    loop {
        match events.try_recv() {
            Ok(e) => last = Some(e),
            Err(async_channel::TryRecvError::Closed) => break,
            Err(async_channel::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    assert_eq!(last, Some(AgentEvent::Exited(Some(0))));
}

/// The child half of [`base_url_comes_from_the_environment`]: runs only with the variable set.
#[test]
fn env_child_prompts_once() {
    if std::env::var_os("LEMMA_TEST_ENV_CHILD").is_none() {
        return;
    }
    let mut cfg = MessagesConfig::new(ApiKey::new(KEY).unwrap());
    assert_eq!(cfg.base_url, None);
    cfg.retry_base = Duration::from_millis(10);
    let mut s = Session::start(cfg, Arc::new(|_| text("ok")));
    s.prompt("From the child");
    assert_eq!(s.turn(), StopReason::EndTurn);
    s.shutdown();
}

#[test]
fn base_url_comes_from_the_environment() {
    if std::env::var_os("LEMMA_TEST_ENV_CHILD").is_some() {
        return;
    }
    let fake = Fake::start(vec![Script::new(OPUS).text(&["hi"]).end("end_turn", 1)]);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "env_child_prompts_once",
            "--quiet",
            "--test-threads=1",
        ])
        .env("LEMMA_TEST_ENV_CHILD", "1")
        .env(messages::BASE_URL_ENV, &fake.url)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let rs = fake.requests();
    assert_eq!(rs.len(), 1);
    assert_eq!(
        rs[0].body["messages"][0]["content"][0]["text"],
        "From the child"
    );
}

#[test]
fn the_key_never_appears_in_logs_events_or_the_raw_log() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("raw.log");
    let fake = Fake::start(vec![
        Reply::Status {
            code: 503,
            headers: vec![("retry-after-ms", "10".into())],
            body: format!("{{\"error\": {{\"message\": \"bad key {KEY}\"}}}}"),
        },
        Script::new(OPUS)
            .tool("toolu_r", "read_file", &["{\"file_path\": \"main.tex\"}"])
            .end("tool_use", 1),
        Script::new(OPUS).text(&["ok"]).end("end_turn", 1),
        Reply::Status {
            code: 401,
            headers: vec![],
            body: format!("{{\"error\": {{\"message\": \"echo {KEY}\"}}}}"),
        },
    ]);
    let mut cfg = config(&fake);
    cfg.raw_log = Some(raw.clone());
    assert!(!format!("{cfg:?}").contains(KEY));
    let mut s = Session::start(cfg, Arc::new(|_| text("x")));
    s.prompt("Hi");
    assert_eq!(s.turn(), StopReason::EndTurn);
    s.prompt("Again");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let events = s.shutdown();
    // The server saw the key (it is the credential) …
    assert_eq!(fake.requests()[0].header("x-api-key"), Some(KEY));
    // … but nothing Lemma shows or writes holds it.
    let shown = format!("{events:?}");
    assert!(shown.contains("<redacted>"), "{shown}");
    assert!(!shown.contains(KEY));
    let raw_log = std::fs::read_to_string(&raw).unwrap();
    assert!(raw_log.contains("main.tex"));
    assert!(!raw_log.contains(KEY));
    let logs = String::from_utf8_lossy(&logs().0.lock().unwrap()).into_owned();
    assert!(logs.contains("api: request"), "the capture works");
    assert!(!logs.contains(KEY));
}

#[test]
fn an_overloaded_error_event_before_any_output_is_retried() {
    let overloaded = sse(
        "error",
        &json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    );
    let fake = Fake::start(vec![
        Reply::Sse(Script::new(OPUS).raw(overloaded.clone()).steps),
        Script::new(OPUS)
            .text(&["Hi"])
            .raw(overloaded)
            .end("end_turn", 1),
    ]);
    let mut s = Session::simple(&fake);
    s.prompt("Hello");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert_eq!(fake.requests().len(), 2);
    assert_eq!(notices(&s.events).len(), 1);
    // After output, the same error ends the turn instead (a retry would repeat the text).
    assert!(
        s.events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error(m) if m.contains("overloaded_error")))
    );
    s.shutdown();
}

// ============================================================================== final review (M5)

fn last_error(events: &[AgentEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        AgentEvent::Error(m) => Some(m.clone()),
        _ => None,
    })
}

/// A 400 answer (here "prompt is too long") must not stay in the history: the next, ordinary
/// prompt is sent without the rejected one and goes through.
#[test]
fn regress5_a_400_does_not_poison_the_session() {
    let fake = Fake::start(vec![
        Reply::Status {
            code: 400,
            headers: vec![],
            body: json!({"type": "error", "error": {"type": "invalid_request_error",
                "message": "prompt is too long"}})
            .to_string(),
        },
        Script::new(OPUS).text(&["Fine."]).end("end_turn", 2),
    ]);
    let mut s = Session::simple(&fake);
    s.prompt("POISON: far too long");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let error = last_error(&s.events).expect("an error");
    assert!(error.contains("(400)"), "{error}");
    s.prompt("an ordinary prompt");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert!(
        s.events.contains(&AgentEvent::MessageChunk("Fine.".into())),
        "{:?}",
        s.events
    );
    let requests = fake.requests();
    assert_eq!(requests.len(), 2);
    let messages = &requests[1].body["messages"];
    assert!(!messages.to_string().contains("POISON"), "{messages}");
    assert_eq!(requests[1].body["messages"].as_array().unwrap().len(), 1);
    s.shutdown();
}

/// A stream that goes silent (no event, not even a ping) is a broken connection after the idle
/// timeout: retried while nothing was shown, an error after output. Never an hour-long hang.
#[test]
fn regress5_a_silent_stream_ends_after_the_idle_timeout() {
    let silent = || Step::Pause(Duration::from_secs(4));
    let fake = Fake::start(vec![
        Reply::Sse(Script::new(OPUS).then(silent()).steps),
        Reply::Sse(
            Script::new(OPUS)
                .text(&["Partial answer"])
                .then(silent())
                .steps,
        ),
    ]);
    let mut cfg = config(&fake);
    cfg.idle_timeout = Duration::from_millis(400);
    let mut s = Session::start(cfg, Arc::new(|_| text("ok")));
    let t0 = Instant::now();
    s.prompt("Hi");
    // The text block of the second response never ends: its chunk is shown at once.
    assert_eq!(s.turn(), StopReason::EndTurn);
    let elapsed = t0.elapsed();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    assert_eq!(
        fake.requests().len(),
        2,
        "the silent first answer is retried"
    );
    let error = last_error(&s.events).expect("an error");
    assert!(error.contains("no data from the API"), "{error}");
    assert_eq!(notices(&s.events).len(), 1, "{:?}", s.events);
    s.shutdown();
}

/// The key the session started with.
static CURRENT_KEY: Mutex<Option<String>> = Mutex::new(None);

fn current_key() -> Option<ApiKey> {
    CURRENT_KEY.lock().unwrap().as_deref().and_then(ApiKey::new)
}

/// After "API key rejected (401)", a key stored since (here: what the lookup returns) is used
/// for the next request; the running session need not be restarted.
#[test]
fn regress5_a_new_key_after_a_401_is_used() {
    const NEW_KEY: &str = "sk-ant-test-NEW-0011223344556677";
    let unauthorized = || Reply::Status {
        code: 401,
        headers: vec![],
        body: json!({"type": "error", "error": {"type": "authentication_error",
            "message": "invalid x-api-key"}})
        .to_string(),
    };
    let fake = Fake::start(vec![
        unauthorized(),
        unauthorized(),
        Script::new(OPUS).text(&["OK answer."]).end("end_turn", 2),
        Script::new(OPUS).text(&["Again."]).end("end_turn", 2),
    ]);
    *CURRENT_KEY.lock().unwrap() = Some(KEY.to_owned());
    let mut cfg = config(&fake);
    cfg.key_lookup = Some(current_key);
    let mut s = Session::start(cfg, Arc::new(|_| text("ok")));
    s.prompt("first");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let error = last_error(&s.events).expect("an error");
    assert!(error.starts_with("API key rejected (401)"), "{error}");
    assert_eq!(fake.requests().len(), 1, "the same key is not sent again");
    // "Set API key…" stores a working key.
    *CURRENT_KEY.lock().unwrap() = Some(NEW_KEY.to_owned());
    s.prompt("second, after the new key");
    assert_eq!(s.turn(), StopReason::EndTurn);
    assert!(
        s.events
            .contains(&AgentEvent::MessageChunk("OK answer.".into())),
        "{:?}",
        s.events
    );
    s.prompt("third");
    assert_eq!(s.turn(), StopReason::EndTurn);
    let keys: Vec<String> = fake
        .requests()
        .iter()
        .map(|r| r.header("x-api-key").unwrap_or_default().to_owned())
        .collect();
    assert_eq!(keys, [KEY, KEY, NEW_KEY, NEW_KEY]);
    s.shutdown();
}
