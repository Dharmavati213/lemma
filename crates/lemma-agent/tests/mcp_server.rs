//! `McpServer` over its unix socket: the token handshake, `tools/call` round trips through the
//! real `lemma-mcp` bridge, concurrency, large messages, timeouts, cancellation, protocol errors,
//! the private socket directory, and `stop`.

mod mcp_support;

use lemma_agent::mcp::{
    MAX_IN_FLIGHT, MAX_MESSAGE_BYTES, McpServer, TIMEOUT_TEXT, Timeouts, tool_definitions,
};
use lemma_agent::{EditorRequest, ToolCall, ToolContent, ToolReply};
use mcp_support::{
    Bridge, RawClient, SocketDir, TIMEOUT, TOKEN, cli_handshake, next_call, raw_exchange, request,
    result_text, spawn_editor, start_server, text_reply, tool_call, wait_until,
};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Timeouts short enough for tests, long enough not to fire by accident.
fn quick() -> Timeouts {
    Timeouts {
        tool: Duration::from_secs(5),
        compile: Duration::from_secs(5),
    }
}

// ---------------------------------------------------------------------------------------------
// Through the real bridge
// ---------------------------------------------------------------------------------------------

#[test]
fn bridge_handshake_then_tools_list_then_tools_call() {
    let (dir, server, rx) = start_server(Timeouts::default());
    let (seen_tx, seen_rx) = mpsc::channel();
    let editor = spawn_editor(rx, move |call| {
        seen_tx.send((call.call, call.req.clone())).unwrap();
        Some(match &call.req {
            EditorRequest::ReadFile { .. } => text_reply("     1\t\\documentclass{article}\n"),
            EditorRequest::RenderPage { page } => ToolReply {
                content: vec![
                    ToolContent::Png(vec![0x89, b'P', b'N', b'G']),
                    ToolContent::Text(format!("PNG copy: /run/lemma/page-{page}.png")),
                ],
                is_error: false,
            },
            other => panic!("unexpected request {other:?}"),
        })
    });
    let mut bridge = Bridge::spawn(&dir.socket(), TOKEN);

    let [discover, initialize, initialized, list] = cli_handshake();
    assert_eq!(
        bridge.call(discover),
        json!({"jsonrpc":"2.0","id":"server-discover-probe-1","result":{}})
    );
    let init = bridge.call(initialize);
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(init["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "lemma");
    // The notification gets no reply: the next line answers tools/list.
    bridge.send(initialized);
    let tools = bridge.call(list);
    assert_eq!(
        tools,
        json!({"jsonrpc":"2.0","id":1,"result":{"tools": tool_definitions()}})
    );

    let reply = bridge.call(&tool_call(
        2,
        "read_file",
        json!({"file_path": "main.tex", "limit": 1}),
    ));
    assert_eq!(
        reply,
        json!({"jsonrpc":"2.0","id":2,"result":{
            "content":[{"type":"text","text":"     1\t\\documentclass{article}\n"}],
            "isError":false
        }})
    );
    let (call, req) = seen_rx.recv_timeout(TIMEOUT).unwrap();
    assert!(call > 0, "call ids start at 1 (0 is the watcher's)");
    assert_eq!(
        req,
        EditorRequest::ReadFile {
            path: PathBuf::from("main.tex"),
            offset: None,
            limit: Some(1),
        }
    );

    let reply = bridge.call(&tool_call("img", "render_page", json!({"page": 2})));
    assert_eq!(
        reply["result"],
        json!({"content":[
            {"type":"image","data":"iVBORw==","mimeType":"image/png"},
            {"type":"text","text":"PNG copy: /run/lemma/page-2.png"}
        ],"isError":false})
    );
    let (second, _) = seen_rx.recv_timeout(TIMEOUT).unwrap();
    assert!(second > call);

    bridge.close_stdin();
    assert!(bridge.wait().success());
    assert!(bridge.remaining_output().is_empty());
    server.stop();
    drop(server);
    assert_eq!(editor.join().unwrap().len(), 2);
}

#[test]
fn bad_token_gets_no_reply_through_the_bridge() {
    let (dir, server, rx) = start_server(quick());
    let mut bridge = Bridge::spawn(&dir.socket(), "fedcba9876543210fedcba9876543210");
    bridge.try_send(&request(1, "tools/list", json!({})));
    bridge.try_send(&tool_call(2, "list_files", json!({})));
    // The server closes the connection after the token line; the bridge exits on its own, with
    // stdin still open, and prints nothing.
    assert!(bridge.wait().success());
    assert!(bridge.remaining_output().is_empty());
    assert!(rx.try_recv().is_err(), "no tool call reached the editor");

    // The server still serves clients with the right token.
    let mut good = Bridge::spawn(&dir.socket(), TOKEN);
    assert_eq!(
        good.call(&request(3, "ping", json!({})))["result"],
        json!({})
    );
    drop(server);
}

#[test]
fn ten_concurrent_bridges_get_their_own_answers() {
    let (dir, server, rx) = start_server(Timeouts::default());
    let socket = dir.socket();
    let clients: Vec<_> = (0..10)
        .map(|i| {
            let socket = socket.clone();
            thread::spawn(move || {
                let mut bridge = Bridge::spawn(&socket, TOKEN);
                let [_, initialize, initialized, list] = cli_handshake();
                assert_eq!(
                    bridge.call(initialize)["result"]["protocolVersion"],
                    "2025-11-25"
                );
                bridge.send(initialized);
                assert_eq!(
                    bridge.call(list)["result"]["tools"]
                        .as_array()
                        .unwrap()
                        .len(),
                    10
                );
                let reply = bridge.call(&tool_call(
                    1000 + i,
                    "write",
                    json!({"file_path": format!("f{i}.tex"), "content": format!("client {i}")}),
                ));
                assert_eq!(reply["id"], 1000 + i);
                assert_eq!(
                    result_text(&reply),
                    (format!("wrote client {i}"), false),
                    "client {i} got someone else's answer"
                );
                bridge.close_stdin();
                assert!(bridge.wait().success());
            })
        })
        .collect();

    // Hold all ten calls before answering any, in reverse order: they are in flight at once.
    let mut calls: Vec<ToolCall> = (0..10).map(|_| next_call(&rx)).collect();
    let mut ids: Vec<u64> = calls.iter().map(|c| c.call).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 10, "every call has its own id");
    while let Some(call) = calls.pop() {
        let EditorRequest::Write { content, .. } = &call.req else {
            panic!("unexpected {:?}", call.req);
        };
        call.reply
            .send(text_reply(format!("wrote {content}")))
            .unwrap();
    }
    for client in clients {
        client.join().unwrap();
    }
    drop(server);
}

#[test]
fn a_five_megabyte_write_arrives_intact_and_a_large_reply_returns_intact() {
    let (dir, server, rx) = start_server(Timeouts::default());
    // LaTeX with every character JSON must escape, multi-byte UTF-8, CRLF and long lines.
    let unit = "\\begin{align}\n  \\mathbb{Z} \\to ℤ, \"quoted\" \t tab\r\n  % 100% ✓ 🦀 \u{7}\\\\\n\\end{align}\n";
    let mut content = String::with_capacity(5 << 20);
    while content.len() < 5 << 20 {
        content.push_str(unit);
    }
    let expected = content.clone();
    let editor = spawn_editor(rx, move |call| {
        let EditorRequest::Write { content, .. } = &call.req else {
            panic!("unexpected {:?}", call.req);
        };
        assert!(*content == expected, "the content changed on the way");
        // Echo it back, so the reply direction carries 5 MB too.
        Some(text_reply(content.clone()))
    });

    let mut bridge = Bridge::spawn(&dir.socket(), TOKEN);
    let reply = bridge.call(&tool_call(
        1,
        "write",
        json!({"file_path": "big.tex", "content": content}),
    ));
    let (text, is_error) = result_text(&reply);
    assert!(!is_error);
    assert!(text == content, "the reply changed on the way back");
    drop(bridge);
    drop(server);
    assert_eq!(editor.join().unwrap().len(), 1);
}

#[test]
fn replies_in_flight_are_delivered_after_the_client_closes_stdin() {
    let (dir, server, rx) = start_server(quick());
    let editor = spawn_editor(rx, |_| {
        thread::sleep(Duration::from_millis(300));
        Some(text_reply("compiled: 0 errors"))
    });
    let mut bridge = Bridge::spawn(&dir.socket(), TOKEN);
    bridge.send(&tool_call(1, "compile", json!({})));
    bridge.close_stdin();
    assert_eq!(
        result_text(&bridge.recv()),
        ("compiled: 0 errors".into(), false)
    );
    assert!(bridge.wait().success());
    drop(server);
    editor.join().unwrap();
}

// ---------------------------------------------------------------------------------------------
// Token handshake
// ---------------------------------------------------------------------------------------------

#[test]
fn connections_without_the_right_token_are_closed_silently() {
    let (dir, server, rx) = start_server(quick());
    let socket = dir.socket();
    let ping = request(1, "ping", json!({}));
    let cases: Vec<Vec<u8>> = vec![
        format!("{{\"lemma_token\":\"{}\"}}\n{ping}\n", TOKEN.to_uppercase()).into_bytes(),
        format!("{{\"lemma_token\":\"{TOKEN}x\"}}\n{ping}\n").into_bytes(),
        format!("{{\"lemma_token\":\"{}\"}}\n{ping}\n", &TOKEN[..31]).into_bytes(),
        format!("{{\"lemma_token\":\"\"}}\n{ping}\n").into_bytes(),
        format!("{{\"token\":\"{TOKEN}\"}}\n{ping}\n").into_bytes(),
        format!("{TOKEN}\n{ping}\n").into_bytes(),
        format!("{ping}\n{ping}\n").into_bytes(),
        format!("\n{{\"lemma_token\":\"{TOKEN}\"}}\n{ping}\n").into_bytes(),
        // A token line longer than the limit, then the real one.
        format!(
            "{{\"lemma_token\":\"{}\"}}\n{{\"lemma_token\":\"{TOKEN}\"}}\n{ping}\n",
            "0".repeat(10_000)
        )
        .into_bytes(),
        // Binary garbage.
        vec![0xff, 0xfe, 0x00, b'\n'],
    ];
    for payload in cases {
        let got = raw_exchange(&socket, &payload);
        assert!(
            got.is_empty(),
            "replied {:?} to {:?}",
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&payload[..payload.len().min(80)])
        );
    }
    // A client that sends nothing and closes, or only part of a line.
    assert!(raw_exchange(&socket, b"").is_empty());
    assert!(raw_exchange(&socket, format!("{{\"lemma_token\":\"{TOKEN}").as_bytes()).is_empty());
    assert!(rx.try_recv().is_err());

    // Whitespace inside the JSON line is fine; the server keeps working after all of the above.
    let mut client =
        RawClient::connect_with(&socket, &format!("  {{ \"lemma_token\" : \"{TOKEN}\" }}  "));
    assert_eq!(client.call(&ping)["result"], json!({}));
    drop(server);
}

// ---------------------------------------------------------------------------------------------
// Protocol on the wire
// ---------------------------------------------------------------------------------------------

#[test]
fn protocol_errors_on_the_wire() {
    let (dir, server, rx) = start_server(quick());
    let mut c = RawClient::connect(&dir.socket());

    let r = c.call("{not json");
    assert_eq!(r["error"]["code"], -32700);
    assert_eq!(r["id"], Value::Null);

    let r = c.call(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#);
    assert_eq!(r["error"]["code"], -32600);

    let r = c.call(&request(2, "resources/list", json!({})));
    assert_eq!(
        (r["id"].clone(), r["error"]["code"].clone()),
        (json!(2), json!(-32601))
    );

    let r = c.call(&tool_call(3, "Edit", json!({})));
    assert_eq!(r["error"]["code"], -32602);
    assert_eq!(r["error"]["message"], "Unknown tool: Edit");

    let r = c.call(&request(4, "tools/call", json!({"arguments": {}})));
    assert_eq!(r["error"]["code"], -32602);
    let r = c.call(&request(5, "tools/call", json!({"name": 5})));
    assert_eq!(r["error"]["code"], -32602);
    let r = c.call(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":["read_file"]}"#);
    assert_eq!(r["error"]["code"], -32602);
    let r =
        c.call(r#"{"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"compile"}}"#);
    assert_eq!(r["error"]["code"], -32600);

    // Bad arguments are a tool execution error, so the model can fix the call; the editor never
    // sees the request.
    let r = c.call(&tool_call(7, "read_file", json!({"path": "main.tex"})));
    assert_eq!(r["id"], 7);
    assert_eq!(
        result_text(&r),
        (
            "read_file: unexpected argument \"path\"; read_file takes file_path, offset, limit"
                .into(),
            true
        )
    );
    let r = c.call(&tool_call(
        8,
        "edit",
        json!({"file_path": "a.tex", "old_string": "x"}),
    ));
    assert_eq!(
        result_text(&r),
        (
            "edit: missing required argument \"new_string\"".into(),
            true
        )
    );
    // No `arguments` at all is fine for a tool without arguments.
    c.send(r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"compile"}}"#);
    let call = next_call(&rx);
    assert_eq!(call.req, EditorRequest::Compile);
    call.reply.send(text_reply("ok")).unwrap();
    let r = c.recv();
    assert_eq!(
        (r["id"].clone(), result_text(&r)),
        (json!(9), ("ok".into(), false))
    );

    // Blank lines, notifications and responses get nothing; CRLF line ends work.
    c.send("");
    c.send("   ");
    c.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    c.send(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
    c.send_raw(b"{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"ping\"}\r\n");
    assert_eq!(c.recv(), json!({"jsonrpc":"2.0","id":10,"result":{}}));
    assert!(rx.try_recv().is_err());
    drop(server);
}

#[test]
fn an_overlong_message_is_refused_and_the_connection_survives() {
    let (dir, server, _rx) = start_server(quick());
    let mut c = RawClient::connect(&dir.socket());
    let mut huge = vec![b'x'; MAX_MESSAGE_BYTES + 1];
    huge.push(b'\n');
    c.send_raw(&huge);
    let r = c.recv();
    assert_eq!(r["error"]["code"], -32600);
    assert_eq!(r["id"], Value::Null);
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("longer than")
    );
    assert_eq!(c.call(&request(1, "ping", json!({})))["result"], json!({}));
    drop(server);
}

#[test]
fn calls_reach_the_editor_in_the_order_they_were_sent() {
    let (dir, server, rx) = start_server(quick());
    let editor = spawn_editor(rx, |call| Some(text_reply(format!("{}", call.call))));
    let mut c = RawClient::connect(&dir.socket());
    // Batches of 20 back-to-back calls (below MAX_IN_FLIGHT, which a slow editor thread could
    // otherwise reach).
    let mut replies: Vec<Value> = Vec::new();
    for batch in 0..5 {
        for i in batch * 20..(batch + 1) * 20 {
            c.send(&tool_call(
                i,
                "edit",
                json!({"file_path": "main.tex", "old_string": format!("#{i}"), "new_string": "x"}),
            ));
        }
        replies.extend((0..20).map(|_| c.recv()));
    }
    replies.sort_by_key(|r| r["id"].as_u64().unwrap());
    drop(c);
    drop(server);
    let seen = editor.join().unwrap();
    let order: Vec<String> = seen
        .iter()
        .map(|call| match &call.req {
            EditorRequest::Edit { old_string, .. } => old_string.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    let expected: Vec<String> = (0..100).map(|i| format!("#{i}")).collect();
    assert_eq!(order, expected);
    assert!(seen.windows(2).all(|w| w[0].call < w[1].call));
    // Each reply went to its own request.
    for (reply, call) in replies.iter().zip(&seen) {
        assert_eq!(result_text(reply).0, call.call.to_string());
    }
}

#[test]
fn ping_is_answered_while_a_compile_runs() {
    let (dir, server, rx) = start_server(quick());
    let mut c = RawClient::connect(&dir.socket());
    c.send(&tool_call("build", "compile", json!({})));
    let compile = next_call(&rx);
    assert_eq!(
        c.call(&request("p", "ping", json!({}))),
        json!({"jsonrpc":"2.0","id":"p","result":{}})
    );
    // A read-only call in parallel is not blocked either.
    c.send(&tool_call("sel", "get_selection", json!({})));
    let selection = next_call(&rx);
    selection.reply.send(text_reply("main.tex:3:1")).unwrap();
    assert_eq!(c.recv()["id"], "sel");
    compile.reply.send(text_reply("built")).unwrap();
    assert_eq!(result_text(&c.recv()), ("built".into(), false));
    drop(server);
}

// ---------------------------------------------------------------------------------------------
// Timeouts, cancellation, editor failures
// ---------------------------------------------------------------------------------------------

#[test]
fn a_call_the_editor_does_not_answer_times_out() {
    let timeouts = Timeouts {
        tool: Duration::from_millis(300),
        compile: Duration::from_secs(3),
    };
    let (dir, server, rx) = start_server(timeouts);
    assert_eq!(server.timeouts(), timeouts);
    let mut bridge = Bridge::spawn(&dir.socket(), TOKEN);

    let start = Instant::now();
    bridge.send(&tool_call(1, "read_file", json!({"file_path": "main.tex"})));
    let held = next_call(&rx); // never answered in time
    let reply = bridge.recv();
    let elapsed = start.elapsed();
    assert_eq!(reply["id"], 1);
    assert_eq!(result_text(&reply), (TIMEOUT_TEXT.into(), true));
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    // The late reply is dropped quietly, and the connection keeps working.
    assert!(held.reply.send(text_reply("too late")).is_err());
    assert_eq!(
        bridge.call(&request(2, "ping", json!({})))["result"],
        json!({})
    );
    assert!(bridge.try_recv(Duration::from_millis(200)).is_none());

    // compile has its own, longer timeout: an answer after 800 ms still arrives...
    bridge.send(&tool_call(3, "compile", json!({})));
    let compile = next_call(&rx);
    thread::sleep(Duration::from_millis(800));
    compile.reply.send(text_reply("built")).unwrap();
    assert_eq!(result_text(&bridge.recv()), ("built".into(), false));
    // ...while any other tool answered that late has already timed out.
    bridge.send(&tool_call(4, "outline", json!({})));
    let outline = next_call(&rx);
    thread::sleep(Duration::from_millis(800));
    let _ = outline.reply.send(text_reply("late"));
    assert_eq!(result_text(&bridge.recv()), (TIMEOUT_TEXT.into(), true));

    // New timeouts apply to calls started afterwards.
    server.set_timeouts(Timeouts {
        tool: Duration::from_secs(5),
        compile: Duration::from_secs(5),
    });
    bridge.send(&tool_call(5, "outline", json!({})));
    let outline = next_call(&rx);
    thread::sleep(Duration::from_millis(800));
    outline.reply.send(text_reply("sections")).unwrap();
    assert_eq!(result_text(&bridge.recv()), ("sections".into(), false));
    drop(server);
}

#[test]
fn a_cancelled_call_gets_no_reply() {
    let (dir, server, rx) = start_server(quick());
    let mut c = RawClient::connect(&dir.socket());
    c.send(&tool_call("slow", "compile", json!({})));
    let held = next_call(&rx);
    c.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"slow","reason":"user interrupted"}}"#);
    // A cancellation of an unknown request is ignored.
    c.send(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":99}}"#);
    thread::sleep(Duration::from_millis(250));
    let _ = held.reply.send(text_reply("built anyway"));
    assert_eq!(
        c.call(&request(1, "ping", json!({}))),
        json!({"jsonrpc":"2.0","id":1,"result":{}})
    );
    assert!(
        c.try_recv(Duration::from_millis(300)).is_none(),
        "no reply for a cancelled call"
    );

    // The same id can be used again after the cancellation.
    c.send(&tool_call("slow", "compile", json!({})));
    next_call(&rx).reply.send(text_reply("built")).unwrap();
    assert_eq!(c.recv()["id"], "slow");
    drop(server);
}

#[test]
fn editor_failures_become_tool_errors() {
    let (dir, server, rx) = start_server(quick());
    let mut c = RawClient::connect(&dir.socket());

    // The editor drops the request without answering.
    c.send(&tool_call(1, "list_files", json!({})));
    drop(next_call(&rx));
    let (text, is_error) = result_text(&c.recv());
    assert!(is_error);
    assert!(text.contains("dropped the request"), "{text}");

    // The editor no longer takes tool calls (its receiver is gone).
    drop(rx);
    let (text, is_error) = result_text(&c.call(&tool_call(2, "list_files", json!({}))));
    assert!(is_error);
    assert!(text.contains("not accepting tool calls"), "{text}");
    drop(server);
}

#[test]
fn too_many_calls_in_flight_on_one_connection_are_refused() {
    let (dir, server, rx) = start_server(quick());
    let mut c = RawClient::connect(&dir.socket());
    for i in 0..MAX_IN_FLIGHT {
        c.send(&tool_call(i, "get_selection", json!({})));
    }
    let held: Vec<ToolCall> = (0..MAX_IN_FLIGHT).map(|_| next_call(&rx)).collect();
    let r = c.call(&tool_call("extra", "get_selection", json!({})));
    assert_eq!(r["id"], "extra");
    let (text, is_error) = result_text(&r);
    assert!(is_error && text.contains("too many tool calls"), "{text}");
    for call in held {
        call.reply.send(text_reply("x")).unwrap();
    }
    for _ in 0..MAX_IN_FLIGHT {
        assert_eq!(result_text(&c.recv()), ("x".into(), false));
    }
    // Room again.
    c.send(&tool_call("again", "get_selection", json!({})));
    next_call(&rx).reply.send(text_reply("y")).unwrap();
    assert_eq!(c.recv()["id"], "again");
    drop(server);
}

// ---------------------------------------------------------------------------------------------
// Socket file and directory
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn the_socket_lives_in_a_private_directory() {
    // A missing directory (and its missing parent) is created with mode 0700.
    let dir = SocketDir::new();
    let socket = dir.0.join("sub").join("mcp.sock");
    let (tx, _rx) = async_channel::unbounded();
    let server = McpServer::start(&socket, TOKEN.into(), tx.clone()).unwrap();
    assert_eq!(server.socket, socket);
    assert_eq!(mode(&dir.0), 0o700);
    assert_eq!(mode(&dir.0.join("sub")), 0o700);
    assert!(
        std::fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(mode(&socket), 0o600);
    drop(server);
    assert!(!socket.exists(), "drop removes the socket");
    assert!(dir.0.join("sub").is_dir(), "the directory stays");

    // An existing directory of ours that others can enter is tightened to 0700.
    let dir = SocketDir::new();
    dir.create(0o755);
    let server = McpServer::start(&dir.socket(), TOKEN.into(), tx.clone()).unwrap();
    assert_eq!(mode(&dir.0), 0o700);
    drop(server);

    // A socket path without a directory, and an empty token, are refused.
    let e =
        McpServer::start(std::path::Path::new("mcp.sock"), TOKEN.into(), tx.clone()).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    let dir = SocketDir::new();
    let e = McpServer::start(&dir.socket(), String::new(), tx.clone()).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    assert!(!dir.socket().exists());

    // A path too long for sun_path is an error, not a panic.
    let long = dir.0.join("x".repeat(120));
    assert!(McpServer::start(&long.join("s.sock"), TOKEN.into(), tx).is_err());
}

#[test]
fn stale_sockets_are_replaced_but_live_ones_and_other_files_are_not() {
    let dir = SocketDir::new();
    dir.create(0o700);
    let socket = dir.socket();
    let (tx, _rx) = async_channel::unbounded();

    // A regular file at the path is left alone.
    std::fs::write(&socket, "not a socket").unwrap();
    let e = McpServer::start(&socket, TOKEN.into(), tx.clone()).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&socket).unwrap(), "not a socket");
    std::fs::remove_file(&socket).unwrap();

    // A stale socket (its listener is gone) is replaced.
    #[cfg(unix)]
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    #[cfg(windows)]
    {
        let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::write(&socket, gone.local_addr().unwrap().to_string()).unwrap();
    }
    assert!(socket.exists());
    let first = McpServer::start(&socket, TOKEN.into(), tx.clone()).unwrap();

    // A live one is not.
    let e = McpServer::start(&socket, TOKEN.into(), tx.clone()).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse);
    let mut c = RawClient::connect(&socket);
    assert_eq!(c.call(&request(1, "ping", json!({})))["result"], json!({}));
    drop(c);

    // Stopping a server never removes a socket that is no longer its own.
    std::fs::remove_file(&socket).unwrap();
    let second = McpServer::start(&socket, TOKEN.into(), tx).unwrap();
    drop(first);
    assert!(
        socket.exists(),
        "the second server's socket survived the first one's drop"
    );
    let mut c = RawClient::connect(&socket);
    assert_eq!(c.call(&request(2, "ping", json!({})))["result"], json!({}));
    drop(second);
    assert!(!socket.exists());
}

// ---------------------------------------------------------------------------------------------
// stop
// ---------------------------------------------------------------------------------------------

#[test]
fn stop_unblocks_the_accept_thread_and_closes_every_connection() {
    let (dir, server, rx) = start_server(Timeouts::default());
    let socket = dir.socket();

    // An idle bridge, a client waiting for a compile the editor never answers, and a connection
    // that has not sent its token yet.
    let mut idle = Bridge::spawn(&socket, TOKEN);
    assert_eq!(
        idle.call(&request(1, "ping", json!({})))["result"],
        json!({})
    );
    let mut busy = Bridge::spawn(&socket, TOKEN);
    busy.send(&tool_call(1, "compile", json!({})));
    let held = next_call(&rx);
    let mut silent = mcp_support::connect(&socket).unwrap();
    silent.set_read_timeout(Some(TIMEOUT)).unwrap();

    let start = Instant::now();
    server.stop();
    let took = start.elapsed();
    assert!(took < Duration::from_secs(1), "stop took {took:?}");
    assert!(server.is_stopped());
    assert!(!socket.exists(), "the socket file is removed");
    assert!(mcp_support::connect(&socket).is_err());

    // Every bridge exits because its socket closed (stdin is still open), and the in-flight
    // call gets no reply.
    assert!(idle.wait().success());
    assert!(busy.wait().success());
    assert!(busy.remaining_output().is_empty());
    let mut buf = [0u8; 16];
    match std::io::Read::read(&mut silent, &mut buf) {
        Ok(n) => assert_eq!(n, 0, "no bytes before the close"),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
    }
    assert!(held.reply.send(text_reply("late")).is_err());

    // Idempotent, and drop after stop is fine.
    server.stop();
    drop(server);
}

#[test]
fn stop_right_after_start_and_a_restart_on_the_same_path() {
    let dir = SocketDir::new();
    let (tx, rx) = async_channel::unbounded();
    for round in 0..20 {
        let server = McpServer::start(&dir.socket(), TOKEN.into(), tx.clone()).unwrap();
        if round % 2 == 0 {
            let mut c = RawClient::connect(&dir.socket());
            c.send(&tool_call(round, "get_selection", json!({})));
            let call = next_call(&rx);
            call.reply.send(text_reply("sel")).unwrap();
            assert_eq!(result_text(&c.recv()), ("sel".into(), false));
        }
        let start = Instant::now();
        server.stop();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(!dir.socket().exists());
    }
}

#[test]
fn a_dropped_server_lets_go_of_the_tool_channel() {
    let (dir, server, rx) = start_server(quick());
    let mut c = RawClient::connect(&dir.socket());
    assert_eq!(c.call(&request(1, "ping", json!({})))["result"], json!({}));
    drop(server);
    c.expect_closed_silently();
    // The server's sender was its only one: the channel is closed once it is gone.
    wait_until("the tool channel to close", || rx.is_closed());
    drop(dir);
}
