//! Process lifecycle of the ACP client: a killed agent gives `Exited` and leaves no thread,
//! process or zombie behind; `Shutdown` joins within a second and the thread count returns to
//! its baseline. One test, so that nothing else runs in this process while threads are counted.
#![cfg(unix)]

mod acp_support;

use acp_support::*;
use lemma_agent::{AgentCmd, AgentEvent, StopReason};
use std::time::{Duration, Instant};

/// No process of `pgrp` is alive (zombies included: they must have been reaped).
fn group_gone(pgrp: i32) -> bool {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()))
        .all(|pid| proc_stat(pid).is_none_or(|(_, _, g)| g != pgrp))
}

/// No child of this process is a zombie.
fn no_zombie_children() -> bool {
    let me = std::process::id() as i32;
    children_of(me)
        .into_iter()
        .all(|pid| proc_stat(pid).is_none_or(|(state, _, _)| state != 'Z'))
}

#[test]
fn lifecycle_leaves_no_threads_processes_or_zombies() {
    // Build the helper binaries before counting threads.
    bins();
    let baseline = thread_count();

    // --- 1. Shutdown with a turn running joins within a second.
    {
        let sb = Sandbox::new("life-shutdown");
        let scenario = fixtures().join("agent/cancel.json");
        let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
        expect_ready(&handle);
        let agent = agent_pid(&scenario);
        assert!(thread_count() > baseline);
        prompt(&handle, "start");
        events_until(&handle, SLOW, |e| matches!(e, AgentEvent::MessageChunk(_)));
        // The agent and its MCP bridge (a child of the agent) share the agent's process group.
        let bridge = children_of(agent)
            .into_iter()
            .find(|p| cmdline(*p).contains("lemma-mcp"))
            .expect("the fake agent started lemma-mcp");
        assert_eq!(proc_stat(bridge).unwrap().2, agent);
        assert_eq!(
            proc_stat(agent).unwrap().2,
            agent,
            "the agent leads its group"
        );

        let t0 = Instant::now();
        handle.cmd.send_blocking(AgentCmd::Shutdown).unwrap();
        let rest = drain(&handle, Duration::from_secs(1));
        let took = t0.elapsed();
        assert!(took < Duration::from_secs(1), "shutdown took {took:?}");
        // (The rest of the streamed message may still arrive first.)
        let rest: Vec<AgentEvent> = rest
            .into_iter()
            .filter(|e| !matches!(e, AgentEvent::MessageChunk(_) | AgentEvent::ThoughtChunk(_)))
            .collect();
        assert_eq!(
            rest,
            [
                AgentEvent::TurnEnded(StopReason::Cancelled),
                AgentEvent::Exited(Some(0))
            ]
        );
        assert!(
            eventually(Duration::from_secs(1), || thread_count() == baseline),
            "threads: {} after shutdown, baseline {baseline}",
            thread_count()
        );
        assert!(group_gone(agent), "the agent's process group is gone");
        assert!(no_zombie_children());
        assert!(handle.cmd.send_blocking(AgentCmd::Cancel).is_err());
    }

    // --- 2. Killing the agent gives Exited, and nothing is left.
    #[cfg(unix)]
    {
        let sb = Sandbox::new("life-kill");
        let scenario = fixtures().join("agent/cancel.json");
        let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
        expect_ready(&handle);
        let agent = agent_pid(&scenario);
        prompt(&handle, "start");
        events_until(&handle, SLOW, |e| matches!(e, AgentEvent::MessageChunk(_)));
        let t0 = Instant::now();
        // SAFETY: plain kill(2) of our own child.
        assert_eq!(unsafe { libc::kill(agent, libc::SIGKILL) }, 0);
        let rest = drain(&handle, SLOW);
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        // The turn ends (no stop reason from a dead agent), then Exited with no code: a signal.
        let tail: Vec<&AgentEvent> = rest
            .iter()
            .filter(|e| !matches!(e, AgentEvent::MessageChunk(_) | AgentEvent::ThoughtChunk(_)))
            .collect();
        assert_eq!(
            tail,
            [
                &AgentEvent::TurnEnded(StopReason::EndTurn),
                &AgentEvent::Exited(None)
            ]
        );
        assert!(
            eventually(Duration::from_secs(1), || thread_count() == baseline),
            "threads: {} after the kill, baseline {baseline}",
            thread_count()
        );
        assert!(group_gone(agent), "the bridge died with the agent");
        assert!(no_zombie_children());
        // Later commands fail.
        assert!(handle.cmd.send_blocking(AgentCmd::Cancel).is_err());
    }

    // --- 3. A crash during start-up is reaped too.
    {
        let sb = Sandbox::new("life-startup");
        let scenario = sb.scenario(&serde_json::json!({
            "_options": {"exit_on": "session/new", "exit_code": 9, "startup_stderr": "fatal"},
            "default": []
        }));
        let handle = lemma_agent::spawn(sb.config(&scenario)).unwrap();
        let events = drain(&handle, SLOW);
        assert!(matches!(&events[0], AgentEvent::Error(m) if m.contains("fatal")));
        assert_eq!(events.last(), Some(&AgentEvent::Exited(Some(9))));
        assert!(
            eventually(Duration::from_secs(1), || thread_count() == baseline),
            "threads: {} after a start-up crash, baseline {baseline}",
            thread_count()
        );
        assert!(no_zombie_children());
    }
}
