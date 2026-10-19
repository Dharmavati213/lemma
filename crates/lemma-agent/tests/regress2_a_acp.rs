//! Round-2 regression tests for the ACP client (area A). Each test drives `lemma_agent::spawn`
//! against a small scripted ACP agent written in Python (`python3` runs it in place of node and
//! the adapter), because these cases need wire-level timing that `lemma-fake-agent` cannot
//! script: several messages in one write right after `session/new`, a `$/cancel_request` for a
//! permission request, and a burst of updates.

mod acp_support;

use acp_support::*;
use lemma_agent::{AgentCmd, AgentConfig, AgentEvent};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The scripted agent. `SCENARIO` is substituted per test.
const AGENT: &str = r#"
import sys, json, threading, time, os
SCENARIO = "@SCENARIO@"
out_lock = threading.Lock()
def write(text):
    with out_lock:
        sys.stdout.write(text)
        sys.stdout.flush()
def line(msg):
    return json.dumps(msg) + "\n"
def send(msg):
    write(line(msg))
SID = "s1"
def update(u):
    return {"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": SID, "update": u}}
def chunk(text):
    return update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})
MODES = [{"id": m, "name": m} for m in ["default", "acceptEdits", "plan", "bypassPermissions"]]
def config(mode):
    return [{"id": "mode", "name": "Mode", "type": "select", "currentValue": mode,
             "options": [{"value": m["id"], "name": m["name"]} for m in MODES]}]
state = {"mode": "default", "set_modes": []}
LOG = os.environ.get("CLAUDE_CODE_EXECUTABLE")
def log(text):
    if LOG:
        with open(LOG, "a") as f:
            f.write(text + "\n")
cancelled = threading.Event()
responses = {}
cond = threading.Condition()
def wait_response(rid, timeout):
    deadline = time.time() + timeout
    with cond:
        while rid not in responses:
            left = deadline - time.time()
            if left <= 0:
                return None
            cond.wait(left)
        return responses[rid]

def turn(pid):
    if SCENARIO == "earlymode":
        send(chunk("agent mode: %s; set_mode requests: %s" % (state["mode"], state["set_modes"])))
        send({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})
    elif SCENARIO == "cancelreq":
        send(update({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "rm -rf x",
                     "kind": "execute", "status": "pending"}))
        send({"jsonrpc": "2.0", "id": 100, "method": "session/request_permission",
              "params": {"sessionId": SID, "toolCall": {"toolCallId": "t1", "title": "rm -rf x"},
                         "options": [{"optionId": "allow-once", "name": "Yes", "kind": "allow_once"},
                                     {"optionId": "reject", "name": "No", "kind": "reject_once"}]}})
        time.sleep(0.3)
        # The CLI aborted the tool use: the adapter withdraws its permission request.
        send({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": 100}})
        got = wait_response(100, 3.0)
        send(chunk("permission response: %s" % (json.dumps(got) if got is not None else "none")))
        send({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})
    elif SCENARIO == "burst":
        # A burst of updates, written as fast as the pipe takes them, then silence.
        block = "".join(line(chunk("x%d " % i)) for i in range(1000))
        for _ in range(int(os.environ.get("BURST_BLOCKS", "300"))):
            if cancelled.is_set():
                break
            write(block)
        cancelled.wait(60)
        send({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "cancelled"}})

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    m = msg.get("method")
    i = msg.get("id")
    if m is None:
        with cond:
            responses[i] = msg
            cond.notify_all()
        continue
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": i, "result": {"protocolVersion": 1,
              "agentCapabilities": {"sessionCapabilities": {"close": {}}},
              "agentInfo": {"name": "scripted", "version": "0"}}})
    elif m == "session/new":
        res = {"sessionId": SID, "modes": {"currentModeId": "default", "availableModes": MODES},
               "configOptions": config("default")}
        if SCENARIO == "earlymode":
            # The response, a chunk and a switch to a mode Lemma does not offer, in one write
            # (ACP allows any session/update once the session exists).
            state["mode"] = "bypassPermissions"
            write(line({"jsonrpc": "2.0", "id": i, "result": res})
                  + line(chunk("early chunk"))
                  + line(update({"sessionUpdate": "current_mode_update",
                                 "currentModeId": "bypassPermissions"})))
        else:
            send({"jsonrpc": "2.0", "id": i, "result": res})
    elif m == "session/set_mode":
        state["mode"] = msg["params"]["modeId"]
        state["set_modes"].append(state["mode"])
        send({"jsonrpc": "2.0", "id": i, "result": {}})
    elif m == "session/prompt":
        threading.Thread(target=turn, args=(i,), daemon=True).start()
    elif m == "session/cancel":
        log("session/cancel %f" % time.time())
        cancelled.set()
    elif m == "session/close":
        cancelled.set()
        send({"jsonrpc": "2.0", "id": i, "result": {}})
    elif i is not None:
        send({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": "not scripted"}})
"#;

fn python() -> PathBuf {
    let path = PathBuf::from("/usr/bin/python3");
    assert!(
        path.is_file(),
        "these tests run a scripted agent with python3"
    );
    path
}

/// A config that runs the scripted agent with `scenario`.
fn scripted(sb: &Sandbox, scenario: &str) -> AgentConfig {
    let script = sb.dir.join(format!("agent-{scenario}.py"));
    std::fs::write(&script, AGENT.replace("@SCENARIO@", scenario)).unwrap();
    let mut cfg = sb.config(&script);
    cfg.program = python();
    // The agent's log (the only path the client passes on: $CLAUDE_CODE_EXECUTABLE).
    cfg.env = vec![(
        "CLAUDE_CODE_EXECUTABLE".into(),
        sb.dir.join("agent.log").into_os_string(),
    )];
    cfg
}

/// A-1: an agent that switches to a mode Lemma does not offer in the same write as its
/// `session/new` answer must be switched back to `default` (risk §9.6), and the UI must not be
/// told `default` while the agent runs in `bypassPermissions`.
#[test]
fn regress2_early_mode_update_is_enforced() {
    let sb = Sandbox::new("r2-earlymode");
    let handle = lemma_agent::spawn(scripted(&sb, "earlymode")).unwrap();
    let ready = expect_ready(&handle);
    prompt(&handle, "report your mode");
    let events = events_until(&handle, SLOW, is_turn_end);
    let text = message_text(&events);
    assert!(
        text.contains("agent mode: default"),
        "the agent still runs in a mode Lemma does not offer.\nReady: {ready:?}\nevents: {events:#?}"
    );
    // The UI learnt of the switch (Ready or a ModeChanged said so, then a Notice) and ends up
    // showing the mode the agent is in.
    let AgentEvent::Ready { mode, .. } = &ready else {
        unreachable!()
    };
    let mut shown = mode.clone();
    let mut modes_seen = vec![mode.clone()];
    for event in &events {
        if let AgentEvent::ModeChanged(m) = event {
            shown.clone_from(m);
            modes_seen.push(m.clone());
        }
    }
    assert!(
        modes_seen.iter().any(|m| m == "bypassPermissions"),
        "the UI was never told of the switch: Ready {ready:?}, events {events:#?}"
    );
    assert_eq!(shown, "default", "Ready {ready:?}, events {events:#?}");
    let notices = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Notice(n) if n.contains("does not offer")))
        .count();
    assert_eq!(notices, 1, "{events:#?}");
    shutdown(&handle);
}

/// A-1, follow-up: the app asks for its preferred mode (`plan`) as soon as `Ready` arrives, while
/// the switch back to `default` may still be in flight: two `session/set_mode` requests at once.
/// With an agent that sends no mode updates of its own, the UI must still end up showing the mode
/// the agent is in.
#[test]
fn regress2_switch_back_and_preferred_mode_agree() {
    let sb = Sandbox::new("r2-earlymode-pref");
    let handle = lemma_agent::spawn(scripted(&sb, "earlymode")).unwrap();
    let ready = expect_ready(&handle);
    send(&handle, AgentCmd::SetMode("plan".into()));
    prompt(&handle, "report your mode");
    let events = events_until(&handle, SLOW, is_turn_end);
    let AgentEvent::Ready { mode, .. } = &ready else {
        unreachable!()
    };
    let mut shown = mode.clone();
    for event in &events {
        if let AgentEvent::ModeChanged(m) = event {
            shown.clone_from(m);
        }
    }
    let text = message_text(&events);
    assert!(
        text.contains(&format!("agent mode: {shown};")),
        "the UI shows {shown:?}, but the agent says {text:?}.\nReady {ready:?}\nevents {events:#?}"
    );
    shutdown(&handle);
}

/// A-2: when the agent withdraws a permission request with `$/cancel_request`, Lemma settles it
/// (a `cancelled` outcome or the request-cancelled error) instead of leaving it answerable.
#[test]
fn regress2_cancelled_permission_request_is_settled() {
    let sb = Sandbox::new("r2-cancelreq");
    let handle = lemma_agent::spawn(scripted(&sb, "cancelreq")).unwrap();
    expect_ready(&handle);
    prompt(&handle, "run it");
    // The card is shown; nobody answers it.
    let shown = events_until(&handle, SLOW, |e| {
        matches!(e, AgentEvent::Permission { .. })
    });
    let Some(AgentEvent::Permission { request, .. }) = shown.last() else {
        unreachable!()
    };
    let request = *request;
    let events = events_until(&handle, SLOW, is_turn_end);
    let text = message_text(&events);
    assert!(
        text.contains("permission response:") && !text.contains("permission response: none"),
        "the withdrawn permission request was never settled: {events:#?}"
    );
    // Settled with the request-cancelled error, and the UI was told to take the card back.
    assert!(text.contains("-32800"), "{text}");
    assert!(
        events.contains(&AgentEvent::PermissionWithdrawn { request }),
        "{events:#?}"
    );
    // A late answer to the withdrawn card goes nowhere (and is no error).
    send(
        &handle,
        AgentCmd::Permission {
            request,
            option_id: Some("allow-once".into()),
        },
    );
    let rest = shutdown(&handle);
    assert!(errors(&rest).is_empty(), "{rest:#?}");
}

/// A-7: `Shutdown` must take effect within about a second (module docs: "This takes well under
/// a second") even while a burst of updates from the agent is still being parsed.
#[test]
fn regress2_shutdown_is_not_starved_by_an_update_burst() {
    let sb = Sandbox::new("r2-burst");
    let handle = lemma_agent::spawn(scripted(&sb, "burst")).unwrap();
    expect_ready(&handle);
    prompt(&handle, "go");
    // The burst (300 000 short updates) is written within about a second.
    events_until(&handle, SLOW, |e| matches!(e, AgentEvent::MessageChunk(_)));
    std::thread::sleep(Duration::from_millis(1500));
    let t0 = Instant::now();
    let shutdown_at = std::time::SystemTime::now();
    send(&handle, AgentCmd::Shutdown);
    let limit = Duration::from_secs(5);
    let mut chunks_after_shutdown = 0usize;
    let mut exited = false;
    while t0.elapsed() < limit {
        match next_event(&handle, Duration::from_millis(50)) {
            Some(AgentEvent::Exited(_)) => {
                exited = true;
                break;
            }
            Some(AgentEvent::MessageChunk(_)) => chunks_after_shutdown += 1,
            Some(_) => {}
            None if handle.events.is_closed() => break,
            None => {}
        }
    }
    if !exited {
        // Diagnose: when did the command loop act on Shutdown (its first step sends
        // session/cancel), and when did the session end?
        let sent_at = shutdown_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let ended = eventually(Duration::from_secs(180), || {
            while let Some(e) = next_event(&handle, Duration::from_millis(1)) {
                if matches!(e, AgentEvent::MessageChunk(_)) {
                    chunks_after_shutdown += 1;
                }
            }
            handle.events.is_closed()
        });
        let log = std::fs::read_to_string(sb.dir.join("agent.log")).unwrap_or_default();
        let cancel_after = log
            .lines()
            .find_map(|l| l.strip_prefix("session/cancel "))
            .and_then(|t| t.parse::<f64>().ok())
            .map(|t| t - sent_at);
        panic!(
            "Shutdown had no effect within {limit:?}. The agent received session/cancel \
             {cancel_after:?} s after Shutdown was sent; the session ended after {:?} (ended: \
             {ended}); {chunks_after_shutdown} chunks were delivered after Shutdown.",
            t0.elapsed()
        );
    }
}
