//! The `lemma` binary end to end with `tools/lemma-fake-agent` (WP2.11): `--demo agent` with
//! the `LEMMA_DEMO_*` knobs of `testmode::agent` answers permission cards with their keys,
//! presses Stop, and waits for changes the file monitor reports after the turn. Each test reads
//! the demo's report line and the files on disk. `scripts/e2e-agent.sh` runs every scenario
//! the same way (and keeps the snapshots).
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_e2e_agent`. The binary
//! inherits the Broadway environment; `LEMMA_CONFIG=/dev/null` keeps the user's config out.

mod common;

use lemma_app::testmode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    common::run(&[
        ("permission_keys_through_the_binary", permission_keys),
        ("stop_cancels_a_pending_permission", stop_cancels_permission),
        ("bypass_writes_after_the_turn_build", bypass_builds),
        ("basic_edits_reach_disk_and_build", basic),
        ("new_file_opens_at_its_first_hunk", new_file_view),
    ])
}

/// Runs the agent demo of `scenario` on a fresh copy of `tests/fixtures/proj` with the extra
/// environment `env`; returns the copy and the report's fields.
fn demo(name: &str, scenario: &str, env: &[(&str, &str)]) -> (PathBuf, HashMap<String, String>) {
    common::require_agent_binaries();
    let dir = common::temp_dir(&format!("e2e-{name}"));
    copy_dir(&common::fixtures().join("proj"), &dir);
    let mut command = Command::new(env!("CARGO_BIN_EXE_lemma"));
    command
        .arg(dir.join("main.tex"))
        .args(["--demo", "agent", "--agent-scenario"])
        .arg(
            common::fixtures()
                .join("agent")
                .join(scenario)
                .canonicalize()
                .unwrap(),
        )
        .args(["--quit-after-ms", "500"])
        .env("LEMMA_CONFIG", "/dev/null")
        .env_remove(testmode::agent::PROMPT_ENV)
        .env_remove(testmode::agent::ANSWER_ENV)
        .env_remove(testmode::agent::CANCEL_ENV);
    if let Some(fake) = testmode::fake_agent_binary() {
        command.env(testmode::FAKE_AGENT_ENV, fake);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().expect("run lemma");
    let stdout = String::from_utf8_lossy(&out.stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "{name}: lemma failed: {:?}",
        out.status
    );
    let line = stdout
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(testmode::agent::REPORT_PREFIX))
        .unwrap_or_else(|| panic!("{name}: no report line"));
    let fields = line
        .split_whitespace()
        .filter_map(|f| f.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    (dir, fields)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn expect(fields: &HashMap<String, String>, want: &[(&str, &str)]) {
    for (key, value) in want {
        assert_eq!(
            fields.get(*key).map(String::as_str),
            Some(*value),
            "{key} in {fields:?}"
        );
    }
}

fn has_latexmk() -> bool {
    Command::new("latexmk")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `y`, `Y` and `n` on the card send the matching option ids, and the turn goes on.
fn permission_keys() {
    for (key, prompt, option) in [
        ("y", "run it once", "allow-once"),
        ("Y", "allow it always", "allow-with-updates"),
        ("n", "reject that", "reject"),
    ] {
        let (_, fields) = demo(
            &format!("perm-{option}"),
            "permission.json",
            &[
                (testmode::agent::ANSWER_ENV, key),
                (testmode::agent::PROMPT_ENV, prompt),
            ],
        );
        // The scenario fails the turn (an error, not EndTurn) when the answer is not `option`.
        expect(
            &fields,
            &[("stop", "EndTurn"), ("permissions", "1"), ("cards", option)],
        );
    }
    // Without a key the run stops at the card, which stays pending for the snapshot.
    let (_, fields) = demo("perm-pending", "permission.json", &[]);
    expect(&fields, &[("stop", "permission"), ("cards", "pending")]);
}

/// Stop during a pending permission request cancels the card and ends the turn quickly.
fn stop_cancels_permission() {
    let (_, fields) = demo(
        "cancel",
        "cancel.json",
        &[
            (testmode::agent::CANCEL_ENV, "800"),
            (testmode::agent::PROMPT_ENV, "permission please"),
        ],
    );
    expect(
        &fields,
        &[
            ("stop", "Cancelled"),
            ("permissions", "1"),
            ("cards", "cancelled"),
            ("hunks", "-"),
        ],
    );
    let ms: u64 = fields["cancelled_ms"].parse().unwrap();
    assert!(ms <= 500, "the turn ended {ms} ms after Stop");
}

/// Writes that bypass the tools become hunks through the file monitor, which reports them after
/// the turn's end; they still get the turn's build.
fn bypass_builds() {
    let latexmk = has_latexmk();
    let (dir, fields) = demo("bypass", "bypass.json", &[]);
    expect(
        &fields,
        &[
            ("stop", "EndTurn"),
            ("hunks", "chapters/yoneda.tex:1,main.tex:1"),
            ("disk", "same"),
        ],
    );
    if latexmk {
        expect(&fields, &[("builds", "1"), ("build", "Success")]);
    } else {
        println!("  latexmk is not installed: not checking the build");
    }
    let yoneda = std::fs::read_to_string(dir.join("chapters/yoneda.tex")).unwrap();
    assert!(yoneda.contains("is fully faithful, hence an embedding."));
}

/// The demo's default run: hunks in two files, on disk, and one build at the turn's end.
fn basic() {
    let latexmk = has_latexmk();
    let (dir, fields) = demo("basic", "basic.json", &[]);
    expect(
        &fields,
        &[
            ("stop", "EndTurn"),
            ("tool_errors", "0"),
            ("hunks", "chapters/yoneda.tex:1,main.tex:2"),
            ("disk", "same"),
        ],
    );
    if latexmk {
        expect(&fields, &[("builds", "1"), ("build", "Success")]);
    }
    let main = std::fs::read_to_string(dir.join("main.tex")).unwrap();
    assert!(main.contains("These notes serve as a test document for Lemma."));
    assert!(main.contains("is treated in detail in~\\cite{Vistoli}."));
}

/// The long scenario writes a new 30-line file and edits it 30 times: the demo's Review step
/// opens it in a new tab at its hunk, which starts at line 1 (a view scrolled before its first
/// layout used to show the file's end).
fn new_file_view() {
    let (_, fields) = demo("long", "long.json", &[]);
    expect(
        &fields,
        &[
            ("stop", "EndTurn"),
            ("chunks", "200"),
            ("hunks", "long-scenario.tex:1"),
            ("disk", "same"),
        ],
    );
    let view = &fields["view"];
    assert!(
        view.starts_with("long-scenario.tex:1-"),
        "the new file's first lines are not on screen: {view}"
    );
}
