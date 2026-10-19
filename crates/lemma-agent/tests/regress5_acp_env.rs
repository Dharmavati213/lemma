//! Final-review regression: the `ANTHROPIC_API_KEY` that the API-key backend reads must not
//! reach the Claude adapter, which would make Claude Code bill that key instead of the user's
//! login. Pi's keys belong to Pi, so a generic adapter keeps it.
//!
//! The only test in this binary: it sets the variable in its own environment before any thread
//! that reads the environment runs.

mod acp_support;

use acp_support::*;
use lemma_agent::AcpFlavor;
use std::time::Duration;

/// What `ANTHROPIC_API_KEY` the adapter sees for `flavor`: `/bin/sh` runs a script in place of
/// node and the adapter, writes the variable to a file and exits.
fn key_seen_by(flavor: AcpFlavor) -> String {
    let sb = Sandbox::new(&format!("r5-env-{flavor:?}"));
    let out = sb.dir.join("env.txt");
    let script = sb.dir.join("adapter.sh");
    std::fs::write(
        &script,
        format!(
            "printf '%s' \"${{ANTHROPIC_API_KEY-unset}}\" > '{0}.tmp' && mv '{0}.tmp' '{0}'\n",
            out.display()
        ),
    )
    .unwrap();
    let mut cfg = sb.config(&script);
    cfg.program = "/bin/sh".into();
    cfg.flavor = flavor;
    let handle = lemma_agent::spawn(cfg).unwrap();
    assert!(
        eventually(Duration::from_secs(10), || out.exists()),
        "the adapter script did not run"
    );
    let _ = drain(&handle, Duration::from_millis(200));
    drop(handle);
    std::fs::read_to_string(&out).unwrap()
}

#[test]
fn regress5_acp_env_key() {
    // SAFETY: the only test of this binary; no other thread reads the environment yet.
    unsafe { std::env::set_var(lemma_agent::keys::ENV_VAR, "sk-ant-test-not-a-real-key") };
    assert_eq!(key_seen_by(AcpFlavor::Claude), "unset");
    assert_eq!(
        key_seen_by(AcpFlavor::Generic),
        "sk-ant-test-not-a-real-key"
    );
}
