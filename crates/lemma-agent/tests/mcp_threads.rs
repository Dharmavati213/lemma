//! `McpServer::stop` leaves no thread behind, even with live bridges, calls the editor never
//! answers and a client that has not sent its token. A test binary of its own, with a single
//! test, so that no other test's threads disturb the count of `/proc/self/task`.
#![cfg(unix)]

mod mcp_support;

use lemma_agent::ToolCall;
use lemma_agent::mcp::Timeouts;
use mcp_support::{
    Bridge, RawClient, TIMEOUT, TOKEN, next_call, request, start_server, thread_names, tool_call,
    wait_until,
};
use serde_json::json;
use std::time::{Duration, Instant};

fn server_threads() -> Vec<String> {
    thread_names()
        .into_iter()
        .filter(|n| n.starts_with("mcp-"))
        .collect()
}

#[test]
fn stop_joins_every_server_thread() {
    // Build the bridge before taking the baseline (it runs cargo once).
    mcp_support::bridge_binary();
    let baseline = thread_names().len();

    for round in 0..3 {
        let (dir, server, rx) = start_server(Timeouts::default());
        let socket = dir.socket();
        let mut bridges: Vec<Bridge> = (0..3).map(|_| Bridge::spawn(&socket, TOKEN)).collect();
        for (i, bridge) in bridges.iter_mut().enumerate() {
            assert_eq!(
                bridge.call(&request(i, "ping", json!({})))["result"],
                json!({})
            );
        }
        // Two calls the editor holds and never answers.
        bridges[0].send(&tool_call(1, "compile", json!({})));
        bridges[1].send(&tool_call(1, "get_selection", json!({})));
        let held: Vec<ToolCall> = (0..2).map(|_| next_call(&rx)).collect();
        // A raw client mid-conversation and one that never sends its token.
        let mut raw = RawClient::connect(&socket);
        assert_eq!(
            raw.call(&request(9, "ping", json!({})))["result"],
            json!({})
        );
        let silent = mcp_support::connect(&socket).unwrap();

        // accept + 5 connections + 2 call waiters.
        wait_until("all server threads to start", || {
            server_threads().len() == 8
        });

        let start = Instant::now();
        server.stop();
        let took = start.elapsed();
        assert!(
            took < Duration::from_secs(1),
            "round {round}: stop took {took:?}"
        );
        // `stop` joined every server thread, but pthread_join returns when the kernel clears
        // the thread's tid, a moment before the task leaves /proc/self/task (on a loaded
        // machine, a joined thread can still be listed right after the join).
        let joined = Instant::now() + TIMEOUT;
        while !server_threads().is_empty() && Instant::now() < joined {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            server_threads().is_empty(),
            "round {round}: left behind {:?}",
            server_threads()
        );

        for bridge in &mut bridges {
            assert!(
                bridge.wait().success(),
                "the bridge exits when its socket closes"
            );
        }
        raw.expect_closed_silently();
        drop((bridges, raw, silent, held, server, dir, rx));
    }

    // Our own helper threads (bridge and client readers) end with their connections.
    let deadline = Instant::now() + TIMEOUT;
    while thread_names().len() > baseline && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        thread_names().len(),
        baseline,
        "threads: {:?}",
        thread_names()
    );
}
