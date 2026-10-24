//! Round-2 regression tests for the MCP server (area A): reply routing for a request serde_json
//! cannot parse, the `render_page` timeout while a build runs, and the handshake time limit.

mod mcp_support;

use lemma_agent::mcp::{HANDSHAKE_TIMEOUT, TIMEOUT_TEXT, Timeouts};
use lemma_agent::{EditorRequest, ToolContent, ToolReply};
use mcp_support::{RawClient, TIMEOUT, spawn_editor, start_server, text_reply};
use serde_json::json;
use std::io::{Read, Write};
use std::thread;
use std::time::{Duration, Instant};

/// A-4: a `tools/call` whose arguments hold a lone UTF-16 surrogate escape (valid JSON per
/// RFC 8259, and what JavaScript's `JSON.stringify` writes for such a string) must be answered
/// under its own id; a reply with `id: null` leaves the CLI's call pending forever.
#[test]
fn regress2_lone_surrogate_call_is_answered_under_its_id() {
    let (_dir, server, rx) = start_server(Timeouts::default());
    let editor = spawn_editor(rx, |_| Some(text_reply("written")));
    let mut client = RawClient::connect(&server.socket);
    client.send(
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"write","arguments":{"file_path":"a.tex","content":"x \ud83d y"}}}"#,
    );
    let reply = client
        .try_recv(TIMEOUT)
        .expect("the server answers the call");
    assert_eq!(
        reply["id"],
        json!(7),
        "the reply cannot be matched to the call: {reply}"
    );
    // Refused as a tool error (the model can send whole characters), never written with U+FFFD.
    assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    // Any other request is handled as if the escape were U+FFFD.
    let reply = client.call(
        r#"{"jsonrpc":"2.0","id":8,"method":"initialize","params":{"protocolVersion":"2025-11-25","clientInfo":{"name":"x \udc00","version":"1"}}}"#,
    );
    assert_eq!(reply["id"], json!(8), "{reply}");
    assert_eq!(
        reply["result"]["protocolVersion"],
        json!("2025-11-25"),
        "{reply}"
    );
    server.stop();
    // The editor's loop ends when the tools channel closes, with the server.
    drop(server);
    drop(client);
    assert!(
        editor.join().unwrap().is_empty(),
        "the refused call reached the editor"
    );
}

/// A-6: the editor's `render_page` first waits for a running build, for as long as `compile`
/// may wait (build timeout + 20 s). The MCP layer must not give up on it sooner than on
/// `compile`, or the model gets "editor did not answer in time" while the editor still renders.
#[test]
fn regress2_render_page_may_wait_for_a_running_build() {
    let timeouts = Timeouts {
        tool: Duration::from_millis(300),
        compile: Duration::from_secs(5),
    };
    let (_dir, server, rx) = start_server(timeouts);
    // The editor answers render_page after the running build finished (1 s): well within the
    // build timeout, which the compile timeout covers.
    let editor = spawn_editor(rx, |call| {
        Some(match &call.req {
            EditorRequest::RenderPage { page } => {
                thread::sleep(Duration::from_secs(1));
                ToolReply {
                    content: vec![
                        ToolContent::Png(vec![0x89, b'P', b'N', b'G']),
                        ToolContent::Text(format!("Page {page} rendered")),
                    ],
                    is_error: false,
                }
            }
            other => panic!("unexpected request {other:?}"),
        })
    });
    let mut client = RawClient::connect(&server.socket);
    let reply = client.call(
        &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"render_page","arguments":{"page":3}}})
        .to_string(),
    );
    let text = reply.to_string();
    assert!(
        !text.contains(TIMEOUT_TEXT),
        "render_page timed out after {:?} although the editor waits for builds up to the \
         compile timeout ({:?}): {reply}",
        timeouts.for_request(&EditorRequest::RenderPage { page: 3 }),
        timeouts.for_request(&EditorRequest::Compile),
    );
    server.stop();
    // The editor's loop ends when the tools channel closes, with the server.
    drop(server);
    drop(client);
    editor.join().unwrap();
}

/// A-8: `HANDSHAKE_TIMEOUT` is "how long a new connection may take to send its token line". A
/// client that sends its token line one byte at a time must still be dropped once it has passed.
#[test]
fn regress2_handshake_timeout_bounds_the_whole_token_line() {
    let (_dir, server, _rx) = start_server(Timeouts::default());
    let mut stream = mcp_support::connect(&server.socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let start = Instant::now();
    let limit = HANDSHAKE_TIMEOUT + Duration::from_secs(3);
    let prefix = br#"{"lemma_token":"0123456789abcdef0123456789abcdef0123456789abcdef"#;
    let mut closed_after = None;
    let mut i = 0;
    while start.elapsed() < limit {
        // One byte every second: each read(2) of the server returns long before its timeout.
        let byte = prefix[i % prefix.len()];
        i += 1;
        if stream.write_all(&[byte]).is_err() {
            closed_after = Some(start.elapsed());
            break;
        }
        if closed(&stream) {
            closed_after = Some(start.elapsed());
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    assert!(
        closed_after.is_some(),
        "a client still sending its token line after {:?} was not dropped (HANDSHAKE_TIMEOUT \
         is {HANDSHAKE_TIMEOUT:?})",
        start.elapsed()
    );
    server.stop();
}

/// True when the peer closed `stream` (a read returns 0 or a reset; the server never writes
/// before the token line is complete).
fn closed(mut stream: &mcp_support::Stream) -> bool {
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}
