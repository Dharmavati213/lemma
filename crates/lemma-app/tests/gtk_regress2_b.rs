//! Second review round, area B (agent integration in the app: bridge, tools, watcher, history,
//! agent_edits): regression tests for verified defects. Each test fails until its defect is
//! fixed.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress2_b`. Tests that
//! build need latexmk (and print `SKIP` without it); the agent tests build `lemma-fake-agent` and
//! `lemma-mcp` first.

mod common;

use adw::prelude::*;
use lemma_agent::{
    AgentEvent, EditorRequest, PromptBlock, StopReason, ToolCall, ToolReply, ToolStatus,
};
use lemma_app::agent::history;
use lemma_app::agent::{AgentController, AgentState, PromptOrigin, bridge, tools, watcher};
use lemma_app::editor::Editor;
use lemma_app::review_ui::{self, Decision};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::{Config, Paths};
use lemma_core::ProjectRoot;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);
const AGENT_TIMEOUT: Duration = Duration::from_secs(60);
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

fn main() -> ExitCode {
    common::run(&[
        (
            "regress2_late_cancel_does_not_end_the_next_turn",
            regress2_late_cancel_does_not_end_the_next_turn,
        ),
        (
            "regress2_agent_written_latexmkrc_does_not_run",
            regress2_agent_written_latexmkrc_does_not_run,
        ),
        (
            "regress2_restoring_an_earlier_own_write_reloads",
            regress2_restoring_an_earlier_own_write_reloads,
        ),
        (
            "regress2_closed_tab_keeps_turn_start_baseline",
            regress2_closed_tab_keeps_turn_start_baseline,
        ),
        (
            "regress2_dirty_buffer_typing_survives_a_turn_write",
            regress2_dirty_buffer_typing_survives_a_turn_write,
        ),
        (
            "regress2_crlf_multiline_edit_and_write",
            regress2_crlf_multiline_edit_and_write,
        ),
        (
            "regress2_undo_to_saved_text_resolves_conflict",
            regress2_undo_to_saved_text_resolves_conflict,
        ),
        (
            "regress2_feedback_survives_an_undelivered_reply",
            regress2_feedback_survives_an_undelivered_reply,
        ),
        (
            "regress2_render_page_outlasts_a_long_build",
            regress2_render_page_outlasts_a_long_build,
        ),
        (
            "regress2_stale_prompt_future_keeps_next_prompt_preparing",
            regress2_stale_prompt_future_keeps_next_prompt_preparing,
        ),
        (
            "regress2_outline_skips_files_outside_the_root",
            regress2_outline_skips_files_outside_the_root,
        ),
        (
            "regress2_history_gc_covers_other_projects",
            regress2_history_gc_covers_other_projects,
        ),
        (
            "regress2_prompt_saves_only_project_buffers",
            regress2_prompt_saves_only_project_buffers,
        ),
        (
            "regress2_restore_dialog_escapes_the_file_name",
            regress2_restore_dialog_escapes_the_file_name,
        ),
        (
            "regress2_pending_history_save_stays_in_its_project",
            regress2_pending_history_save_stays_in_its_project,
        ),
        (
            "regress2_root_move_keeps_the_agent_in_the_project",
            regress2_root_move_keeps_the_agent_in_the_project,
        ),
        (
            "regress2_list_files_does_not_stall_the_main_thread",
            regress2_list_files_does_not_stall_the_main_thread,
        ),
        (
            "regress2_agent_save_inside_an_insert_keeps_modified_honest",
            regress2_agent_save_inside_an_insert_keeps_modified_honest,
        ),
        (
            "regress2_big_file_written_in_a_turn_does_not_freeze",
            regress2_big_file_written_in_a_turn_does_not_freeze,
        ),
        (
            "regress2_render_page_does_not_retain_memory",
            regress2_render_page_does_not_retain_memory,
        ),
    ])
}

// ----- fixtures (as in gtk_agent_tools.rs) -----

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

fn block_on_timeout<F: Future>(timeout: Duration, f: F) -> F::Output {
    block_on(glib::future_with_timeout(timeout, f)).expect("timed out")
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
    home: PathBuf,
    runtime: PathBuf,
}

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    fixture_with(name, files, |_| {})
}

fn fixture_with(
    name: &str,
    files: &[(&str, &str)],
    configure: impl FnOnce(&mut Config),
) -> Fixture {
    let dir = common::temp_dir(name);
    write_files(&dir, files);
    window_on(dir, configure)
}

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn window_on(dir: PathBuf, configure: impl FnOnce(&mut Config)) -> Fixture {
    window_on_home(dir, |_| {}, configure)
}

/// The fixture's home directory for `dir` (recreated empty by [`window_on`]).
fn home_of(dir: &Path) -> PathBuf {
    PathBuf::from(format!("{}-home", dir.display()))
}

/// [`window_on`], with `prepare` run on the fresh home directory before the window opens.
fn window_on_home(
    dir: PathBuf,
    prepare: impl FnOnce(&Paths),
    configure: impl FnOnce(&mut Config),
) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    thread_local! {
        static NEXT: Cell<u32> = const { Cell::new(0) };
    }
    let n = NEXT.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    let home = home_of(&dir);
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let runtime = glib::user_runtime_dir().join(format!("lemma-r2b{}-{n}", std::process::id()));
    let (home_var, runtime_var) = (home.clone(), runtime.clone());
    let paths = Paths::from_vars(move |name| match name {
        "HOME" => Some(home_var.clone().into()),
        "XDG_RUNTIME_DIR" => Some(runtime_var.clone().into()),
        _ => None,
    })
    .unwrap();
    prepare(&paths);
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(
        &common::application(),
        Rc::new(config),
        Some(paths),
        ws,
        true,
    );
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    let f = Fixture {
        state,
        dir: dir.canonicalize().unwrap(),
        home,
        runtime,
    };
    common::pump_until(TIMEOUT, || watcher::watched_dirs(&f.state).contains(&f.dir));
    f
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(agent) = self.state.agent_controller() {
            agent.stop();
        }
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        block_on(self.state.open_file(&self.dir.join(file))).expect("open")
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn disk(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }

    fn agent(&self) -> Rc<AgentController> {
        self.state.agent_controller().expect("installed").clone()
    }

    fn call(&self, req: EditorRequest) -> ToolReply {
        thread_local! {
            static CALL: Cell<u64> = const { Cell::new(100) };
        }
        let call = CALL.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        block_on(tools::handle(
            self.state.clone(),
            ToolCall {
                call,
                req,
                reply: tx,
            },
        ));
        rx.try_recv().expect("a reply")
    }

    fn call_text(&self, req: EditorRequest) -> String {
        let reply = self.call(req);
        assert!(
            !reply.is_error,
            "tool failed: {}",
            tools::reply_text(&reply)
        );
        tools::reply_text(&reply)
    }

    fn external_write(&self, file: &str, text: &str) {
        let path = self.path(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// As `sed -i` or `git checkout` write: a new file renamed over the old one.
    fn external_replace(&self, file: &str, text: &str) {
        let path = self.path(file);
        let tmp = path.with_file_name("sedXyZ12");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(tmp, path).unwrap();
    }

    fn wait_watcher(&self) {
        common::pump_for(Duration::from_millis(50));
        common::pump_until(TIMEOUT, || watcher::is_idle(&self.state));
    }

    fn events(&self) -> Rc<RefCell<Vec<AgentEvent>>> {
        let events: Rc<RefCell<Vec<AgentEvent>>> = Rc::default();
        let sink = events.clone();
        self.agent()
            .connect_event(move |e| sink.borrow_mut().push(e.clone()));
        events
    }

    fn start_turn(&self, text: &str) -> (u32, Vec<PromptBlock>) {
        block_on(bridge::start_turn(
            &self.agent(),
            vec![PromptBlock::Text(text.into())],
            PromptOrigin::Panel,
        ))
        .expect("the turn starts")
    }

    fn history_dir(&self) -> PathBuf {
        self.state.paths.as_ref().unwrap().history_dir()
    }
}

fn edit(path: &str, old: &str, new: &str) -> EditorRequest {
    EditorRequest::Edit {
        path: path.into(),
        old_string: old.into(),
        new_string: new.into(),
        replace_all: false,
    }
}

fn have_latexmk() -> bool {
    let found = Command::new("latexmk")
        .arg("-v")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        println!("  SKIP: latexmk is not installed");
    }
    found
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Lemma Test",
            "-c",
            "user.email=t@example.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn hunks(f: &Fixture, file: &str) -> usize {
    f.state
        .workspace
        .get(&f.path(file))
        .map_or(0, |d| review_ui::hunk_count(&d))
}

fn baseline(f: &Fixture, file: &str) -> Option<String> {
    let doc = f.state.workspace.get(&f.path(file))?;
    let review = doc.review.borrow();
    review.as_ref().map(|r| r.baseline().to_owned())
}

/// A scenario file written for one test.
fn scenario_file(f: &Fixture, json: &str) -> PathBuf {
    let path = f.home.join("scenario.json");
    std::fs::write(&path, json).unwrap();
    path
}

/// The pids of our child processes named `lemma-fake-agen…` (comm is cut at 15 bytes).
fn fake_agent_pids() -> Vec<i32> {
    let me = std::process::id().to_string();
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let comm = &stat[open + 1..close];
        let ppid = stat[close + 2..].split(' ').nth(1).unwrap_or("");
        if comm.starts_with("lemma-fake-agen") && ppid == me {
            pids.push(stat[..open].trim().parse().unwrap());
        }
    }
    pids
}

#[cfg(unix)]
fn signal(pids: &[i32], sig: i32) {
    for pid in pids {
        // SAFETY: plain kill(2) on our own child.
        unsafe { libc::kill(*pid, sig) };
    }
}

// ----- B-1 (B-2, B-7): the cancel watchdog and the agent's view of the turn -----

/// The agent is slow to confirm a cancel (here: paused, as an adapter whose interrupt queues
/// behind another request). After CANCEL_GRACE the app ends the turn and takes the next prompt;
/// lemma-agent still runs the old turn and refuses that prompt ("still working", no TurnEnded),
/// yet the app shows the new turn running, and the old turn's late TurnEnded ends it.
fn regress2_late_cancel_does_not_end_the_next_turn() {
    common::require_agent_binaries();
    let f = fixture("late-cancel", &[("main.tex", "x\n")]);
    let agent = f.agent();
    agent.set_scenario(Some(
        common::fixtures()
            .join("agent/cancel.json")
            .canonicalize()
            .unwrap(),
    ));
    let events = f.events();
    assert_eq!(
        block_on_timeout(AGENT_TIMEOUT, agent.ensure_started()),
        Ok(())
    );
    agent.prompt(vec![PromptBlock::Text("one".into())], PromptOrigin::Panel);
    let streamed = |events: &Rc<RefCell<Vec<AgentEvent>>>| {
        events
            .borrow()
            .iter()
            .filter(|e| matches!(e, AgentEvent::MessageChunk(t) if t.contains("task")))
            .count()
    };
    common::pump_until(AGENT_TIMEOUT, || streamed(&events) >= 1);
    let pids = fake_agent_pids();
    assert_eq!(pids.len(), 1, "one fake agent");
    // The agent stops answering for a while.
    #[cfg(unix)]
    signal(&pids, libc::SIGSTOP);
    agent.cancel();
    common::pump_until(TIMEOUT, || !agent.is_running());
    agent.prompt(vec![PromptBlock::Text("two".into())], PromptOrigin::Panel);
    common::pump_for(Duration::from_millis(1500));
    let refused = events.borrow().iter().any(
        |e| matches!(e, AgentEvent::Error(m) if m.contains("still working on the previous prompt")),
    );
    let running_while_refused = agent.is_running();
    println!(
        "  second prompt refused by the agent: {refused}; app state then: {:?}",
        agent.state()
    );
    #[cfg(unix)]
    signal(&pids, libc::SIGCONT);
    common::pump_for(Duration::from_millis(1500));
    println!(
        "  after the agent resumed: {:?}; turn-text chunks {}",
        agent.state(),
        streamed(&events)
    );
    if refused {
        assert!(
            !running_while_refused,
            "the agent refused the second prompt, but the app shows its turn as running"
        );
    } else {
        // The prompt was held back until the old turn ended: the agent runs it now.
        common::pump_until(TIMEOUT, || streamed(&events) >= 2);
        assert!(agent.is_running(), "the second turn runs");
    }
}

// ----- B-3: an agent-written latexmkrc runs at the next build -----

fn regress2_agent_written_latexmkrc_does_not_run() {
    if !have_latexmk() {
        return;
    }
    let f = fixture(
        "latexmkrc",
        &[(
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n",
        )],
    );
    f.open("main.tex");
    let marker = f.path("ran-from-latexmkrc");
    let reply = f.call(EditorRequest::Write {
        path: "latexmkrc".into(),
        content: "system('touch ran-from-latexmkrc');\n".into(),
    });
    println!("  write latexmkrc: {:?}", tools::reply_text(&reply));
    let compiled = f.call(EditorRequest::Compile);
    println!("  compile: {:?}", tools::reply_text(&compiled));
    assert!(
        !marker.exists(),
        "Perl the agent wrote through an auto-allowed tool ran at the build, before any review"
    );
}

// ----- B-4 (B-24): an older own write restored by another program -----

fn regress2_restoring_an_earlier_own_write_reloads() {
    let f = fixture("own-old", &[("main.tex", "v0\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    buffer.set_text("v1 committed\n");
    assert_eq!(block_on(doc.save()), Ok(()));
    buffer.set_text("v2 later work\n");
    assert_eq!(block_on(doc.save()), Ok(()));
    f.wait_watcher();
    // `git checkout -- main.tex`: the committed version comes back.
    f.external_replace("main.tex", "v1 committed\n");
    common::pump_for(Duration::from_millis(400));
    f.wait_watcher();
    println!(
        "  disk={:?} buffer={:?} dirty={} bar={}",
        f.disk("main.tex"),
        doc.text(),
        doc.is_dirty(),
        watcher::conflict_bar_shown(&f.state)
    );
    assert_eq!(
        doc.text(),
        "v1 committed\n",
        "a clean buffer reloads what another program wrote"
    );
}

// ----- B-5: a document that left the workspace during the turn -----

fn regress2_closed_tab_keeps_turn_start_baseline() {
    let f = fixture("closed-tab", &[("main.tex", "m\n"), ("ch.tex", "one\n")]);
    if !(git(&f.dir, &["init", "-q"])
        && git(&f.dir, &["add", "ch.tex"])
        && git(&f.dir, &["commit", "-q", "-m", "init"]))
    {
        println!("  SKIP: git is not usable");
        return;
    }
    // Saved, not committed.
    f.external_write("ch.tex", "one\nuser work not committed\n");
    f.wait_watcher();
    f.open("main.tex");
    let ch = f.open("ch.tex");
    assert_eq!(ch.document().text(), "one\nuser work not committed\n");
    f.start_turn("go");
    // The user closes the chapter's tab during the turn.
    f.state.close_editor(&ch, false);
    drop(ch);
    common::pump_for(Duration::from_millis(100));
    // The agent's Bash appends a line.
    f.external_write("ch.tex", "one\nuser work not committed\nagent line\n");
    common::pump_until(TIMEOUT, || hunks(&f, "ch.tex") > 0);
    f.wait_watcher();
    println!("  baseline={:?}", baseline(&f, "ch.tex"));
    assert_eq!(
        baseline(&f, "ch.tex").as_deref(),
        Some("one\nuser work not committed\n"),
        "the change is reviewed against the text at the turn's start"
    );
}

// ----- B-6 (B-26): unsaved typing during a turn -----

fn regress2_dirty_buffer_typing_survives_a_turn_write() {
    let f = fixture("dirty-turn", &[("main.tex", "alpha\nbeta\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    f.start_turn("go");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "USER TYPING ");
    assert!(doc.is_dirty());
    f.external_replace("main.tex", "alpha\nbeta\ngamma\n");
    common::pump_for(Duration::from_millis(400));
    f.wait_watcher();
    let old: Option<Vec<String>> = doc
        .review
        .borrow()
        .as_ref()
        .map(|r| r.hunks().iter().map(|h| h.old_text.clone()).collect());
    println!(
        "  buffer={:?} dirty={} hunks old_text={old:?}",
        doc.text(),
        doc.is_dirty()
    );
    assert!(
        doc.text().contains("USER TYPING "),
        "the user's unsaved typing was removed as an agent edit"
    );
}

// ----- B-8: CRLF files -----

fn regress2_crlf_multiline_edit_and_write() {
    let f = fixture("crlf", &[("main.tex", "alpha\r\nbeta\r\ngamma\r\n")]);
    let read = f.call_text(EditorRequest::ReadFile {
        path: "main.tex".into(),
        offset: None,
        limit: None,
    });
    println!("  read_file: {read:?}");
    let reply = f.call(edit("main.tex", "alpha\nbeta", "ALPHA\nbeta"));
    println!("  multi-line edit: {:?}", tools::reply_text(&reply));
    f.call_text(EditorRequest::Write {
        path: "chapter.tex".into(),
        content: String::new(),
    });
    std::fs::write(f.path("other.tex"), "one\r\ntwo\r\nthree\r\n").unwrap();
    f.call_text(EditorRequest::Write {
        path: "other.tex".into(),
        content: "one\ntwo\nTHREE\n".into(),
    });
    let old: Option<Vec<String>> = f.state.workspace.get(&f.path("other.tex")).and_then(|d| {
        d.review
            .borrow()
            .as_ref()
            .map(|r| r.hunks().iter().map(|h| h.old_text.clone()).collect())
    });
    println!(
        "  write: hunks={old:?} disk={:?}",
        std::fs::read_to_string(f.path("other.tex")).unwrap()
    );
    assert!(
        !reply.is_error,
        "an edit whose old_string spans lines must match a CRLF file"
    );
    assert_eq!(
        old.as_deref(),
        Some(&["three\r\n".to_owned()][..]),
        "a write that changes one line of a CRLF file changes that line only"
    );
}

// ----- B-9: undo back to the saved text while the conflict bar is up -----

fn regress2_undo_to_saved_text_resolves_conflict() {
    let f = fixture("conflict-undo", &[("main.tex", "saved\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.end_iter(), "typed\n");
    assert!(doc.is_dirty());
    f.external_write("main.tex", "other program\n");
    common::pump_until(TIMEOUT, || watcher::conflict_bar_shown(&f.state));
    // The user takes the typing back.
    buffer.undo();
    common::pump_for(Duration::from_millis(300));
    assert!(!doc.is_dirty());
    let bar = watcher::conflict_bar_shown(&f.state);
    println!(
        "  after undo: buffer={:?} bar={bar} conflicts={:?}",
        doc.text(),
        watcher::conflicts(&f.state)
    );
    if bar {
        let reloaded = block_on(watcher::resolve_conflict(
            &f.state,
            &f.path("main.tex"),
            watcher::Resolution::Reload,
        ));
        println!("  Reload: {reloaded:?}");
        assert_eq!(reloaded, Ok(()), "the bar's Reload works");
    }
    assert_eq!(
        doc.text(),
        f.disk("main.tex"),
        "the other program's version is loaded"
    );
}

// ----- B-10: feedback drained into a reply nobody receives -----

fn regress2_feedback_survives_an_undelivered_reply() {
    let f = fixture("feedback-lost", &[("main.tex", "a\nb\nc\n")]);
    f.call_text(edit("main.tex", "b\n", "B\n"));
    let doc = f.state.workspace.get(&f.path("main.tex")).unwrap();
    let id = doc.review.borrow().as_ref().unwrap().hunks()[0].id;
    review_ui::decide(&f.state, &doc, id, Decision::Reject).unwrap();
    // A call the MCP thread gave up on (timed out, or cancelled by Stop): its receiver is gone.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    drop(rx);
    block_on(tools::handle(
        f.state.clone(),
        ToolCall {
            call: 9,
            req: EditorRequest::ReviewState,
            reply: tx,
        },
    ));
    let text = f.call_text(EditorRequest::ReviewState);
    println!("  next report: {text:?}");
    assert!(
        text.contains("main.tex: the user rejected line 2"),
        "the reject is reported to the agent"
    );
}

// ----- B-11 (B-17): render_page during a long build -----

fn regress2_render_page_outlasts_a_long_build() {
    if !have_latexmk() {
        return;
    }
    common::require_agent_binaries();
    let dir = common::temp_dir("render-long");
    // A build that takes 35 s (a thesis with biber and TikZ), then the real latexmk.
    let slow = dir.join("slow-latexmk");
    std::fs::write(&slow, "#!/bin/sh\nsleep 35\nexec latexmk \"$@\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let proj = dir.join("proj");
    write_files(
        &proj,
        &[(
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n",
        )],
    );
    let slow_path = slow.clone();
    let f = window_on(proj, move |c| c.build.latexmk = slow_path);
    f.open("main.tex");
    let agent = f.agent();
    agent.set_scenario(Some(scenario_file(
        &f,
        r#"{"default": [{"tool": {"name": "render_page", "args": {"page": 1}}}]}"#,
    )));
    let events = f.events();
    let timeline: Rc<RefCell<Vec<(Duration, String)>>> = Rc::default();
    let clock = Instant::now();
    let sink = timeline.clone();
    agent.connect_event(move |e| {
        if matches!(
            e,
            AgentEvent::ToolCall { .. }
                | AgentEvent::ToolUpdate { .. }
                | AgentEvent::Error(_)
                | AgentEvent::TurnEnded(_)
        ) {
            sink.borrow_mut().push((clock.elapsed(), format!("{e:?}")));
        }
    });
    assert_eq!(
        block_on_timeout(AGENT_TIMEOUT, agent.ensure_started()),
        Ok(())
    );
    f.state.request_build(lemma_app::build_ui::Reason::Manual);
    let build = f.state.build.get().unwrap();
    common::pump_until(TIMEOUT, || build.is_busy());
    let started = Instant::now();
    agent.prompt(
        vec![PromptBlock::Text("how does page 1 look?".into())],
        PromptOrigin::Panel,
    );
    common::pump_until(BUILD_TIMEOUT, || {
        events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnEnded(_)))
    });
    for (at, e) in timeline.borrow().iter() {
        println!("  [{at:.1?}] {}", e.chars().take(300).collect::<String>());
    }
    let failed = events.borrow().iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolUpdate {
                status: Some(ToolStatus::Failed),
                ..
            } | AgentEvent::Error(_)
        )
    });
    common::pump_until(BUILD_TIMEOUT, || {
        agent
            .bridge()
            .tool_records()
            .iter()
            .any(|r| r.tool == "render_page")
    });
    let record = agent
        .bridge()
        .tool_records()
        .into_iter()
        .find(|r| r.tool == "render_page")
        .unwrap();
    println!(
        "  turn ended after {:.1?}; agent saw the call fail: {failed}; editor answered after \
         {:.1?}: is_error={} {:?}",
        started.elapsed(),
        record.elapsed,
        record.is_error,
        record.text
    );
    assert!(
        !failed,
        "render_page timed out for the agent while the editor was still waiting for the build"
    );
}

// ----- B-13: Stop while a prompt is prepared, then two prompts during the restart -----

fn regress2_stale_prompt_future_keeps_next_prompt_preparing() {
    common::require_agent_binaries();
    let dir = common::temp_dir("stop-race");
    // Enough files that the turn snapshot takes a moment.
    for i in 0..3000 {
        let sub = dir.join(format!("d{}", i / 100));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(format!("f{i}.tex")), "x\n".repeat(200)).unwrap();
    }
    std::fs::write(dir.join("main.tex"), "m\n").unwrap();
    let f = window_on(dir, |_| {});
    let agent = f.agent();
    agent.set_scenario(Some(scenario_file(
        &f,
        r#"{"_options": {"session_new_delay_ms": 1500}, "default": [{"say": "ok"}]}"#,
    )));
    assert_eq!(
        block_on_timeout(AGENT_TIMEOUT, agent.ensure_started()),
        Ok(())
    );
    let events = f.events();
    let prompts: Rc<Cell<u32>> = Rc::default();
    let count = prompts.clone();
    agent.connect_prompt(move |_| count.set(count.get() + 1));
    agent.prompt(vec![PromptBlock::Text("A".into())], PromptOrigin::Panel);
    // A is in its turn snapshot (the jobs thread).
    common::pump_until(TIMEOUT, || agent.is_running());
    agent.stop();
    agent.prompt(vec![PromptBlock::Text("B".into())], PromptOrigin::Panel);
    // A's future ends (its turn was stopped); B waits for the agent to start again.
    common::pump_for(Duration::from_millis(600));
    assert_eq!(agent.state(), AgentState::Starting, "B restarts the agent");
    agent.prompt(vec![PromptBlock::Text("C".into())], PromptOrigin::Panel);
    let c_refused = events.borrow().iter().any(
        |e| matches!(e, AgentEvent::Error(m) if m.contains("still working on the previous prompt")),
    );
    common::pump_until(AGENT_TIMEOUT, || {
        events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnEnded(StopReason::EndTurn)))
    });
    common::pump_for(Duration::from_millis(300));
    let errors: Vec<String> = events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Error(m) => Some(m.clone()),
            _ => None,
        })
        .collect();
    println!(
        "  C refused: {c_refused}; prompts announced: {}; errors: {errors:?}",
        prompts.get()
    );
    assert!(
        c_refused,
        "C must be refused while B is still being prepared"
    );
}

// ----- B-14: outline of files outside the root -----

fn regress2_outline_skips_files_outside_the_root() {
    let base = common::temp_dir("outline-outside");
    write_files(
        &base,
        &[
            (
                "proj/main.tex",
                "\\documentclass{article}\n\\begin{document}\n\\input{../secret}\n\\end{document}\n",
            ),
            (
                "secret.tex",
                "\\section{Private plans}\n\\label{sec:secret}\n",
            ),
        ],
    );
    let f = window_on(base.join("proj"), |_| {});
    let whole = f.call_text(EditorRequest::Outline { path: None });
    let direct = f.call(EditorRequest::Outline {
        path: Some("../secret.tex".into()),
    });
    println!(
        "  outline(): {whole:?}\n  outline(../secret.tex): {:?}",
        tools::reply_text(&direct)
    );
    assert!(direct.is_error);
    assert!(
        !whole.contains("Private plans"),
        "outline() reveals a file outside the project root"
    );
}

// ----- B-15: history caps of other projects -----

fn regress2_history_gc_covers_other_projects() {
    let other_root = common::temp_dir("history-other-proj");
    let dir = common::temp_dir("history-gc");
    write_files(&dir, &[("main.tex", "m\n")]);
    let text = "old\n";
    let hash = lemma_core::fnv1a64(&[text.as_bytes()]);
    let other_dir: Rc<RefCell<PathBuf>> = Rc::default();
    let slot = other_dir.clone();
    let other_main = other_root.join("main.tex");
    // A project last opened 40 days ago, and never again since.
    let f = window_on_home(
        dir,
        move |paths| {
            let other = history::project_dir(&paths.history_dir(), &other_root);
            std::fs::create_dir_all(other.join("objects")).unwrap();
            std::fs::write(other.join(format!("objects/{hash:016x}")), text).unwrap();
            let unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 40 * 86_400;
            let line = format!(
                "{{\"path\":\"{}\",\"hash\":\"{hash:016x}\",\"unix\":{unix},\"reason\":\"save\"}}\n",
                other_main.display()
            );
            std::fs::write(other.join("index.jsonl"), line).unwrap();
            slot.replace(other);
        },
        |_| {},
    );
    // Start-up garbage collection has run.
    block_on(history::flush(&f.state));
    let other = other_dir.borrow().clone();
    let index = std::fs::read_to_string(other.join("index.jsonl")).unwrap_or_default();
    println!("  the other project's index after start-up: {index:?}");
    assert!(
        index.is_empty(),
        "an entry older than history.max_days (30) survives start-up"
    );
}

// ----- B-16: save before the prompt -----

fn regress2_prompt_saves_only_project_buffers() {
    let base = common::temp_dir("save-scope");
    write_files(
        &base,
        &[("proj/main.tex", "m\n"), ("notes/draft.tex", "draft\n")],
    );
    let f = window_on(base.join("proj"), |_| {});
    f.open("main.tex");
    let draft = block_on(f.state.open_file(&base.join("notes/draft.tex"))).unwrap();
    assert_eq!(f.state.workspace.root().path(), f.dir, "the root stays");
    let buffer = draft.buffer();
    buffer.insert(&mut buffer.end_iter(), "unsaved idea\n");
    f.start_turn("go");
    let on_disk = std::fs::read_to_string(base.join("notes/draft.tex")).unwrap();
    println!("  draft.tex on disk after the prompt: {on_disk:?}");
    assert_eq!(
        on_disk, "draft\n",
        "a file outside the project is not saved by the prompt"
    );
}

// ----- B-21: markup in the restore dialog -----

fn regress2_restore_dialog_escapes_the_file_name() {
    let f = fixture_with("restore-markup", &[("a&b <v2>.tex", "x\n")], |c| {
        c.history.enabled = false;
    });
    f.open("a&b <v2>.tex");
    let before = common::logged_problems();
    gtk::prelude::WidgetExt::activate_action(&f.state.window, "win.history-restore", None).unwrap();
    common::pump_for(Duration::from_millis(500));
    let logged = common::logged_problems() - before;
    println!("  GLib warnings while showing the dialog: {logged}");
    // The harness counts the warnings too; say why here.
    assert_eq!(logged, 0, "the file name was parsed as Pango markup");
}

// ----- B-22: a coalesced save waiting when the root moves -----

fn regress2_pending_history_save_stays_in_its_project() {
    let base = common::temp_dir("history-root-move");
    write_files(&base, &[("a/main.tex", "v0\n"), ("b/main.tex", "b\n")]);
    let f = window_on(base.join("a"), |c| c.history.min_interval_s = 2);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    buffer.set_text("v1\n");
    assert_eq!(block_on(doc.save()), Ok(()));
    buffer.set_text("v2\n");
    // Within the interval: written when it is over.
    assert_eq!(block_on(doc.save()), Ok(()));
    f.state.close_editor(&editor, false);
    drop(editor);
    drop(doc);
    common::pump_for(Duration::from_millis(100));
    assert!(f.state.workspace.is_empty());
    // The next file opened moves the root to project b.
    let b_root = base.join("b").canonicalize().unwrap();
    block_on(f.state.open_file(&b_root.join("main.tex"))).unwrap();
    assert_eq!(f.state.workspace.root().path(), b_root);
    common::pump_for(Duration::from_millis(2500));
    block_on(history::flush(&f.state));
    let b_index = history::project_dir(&f.history_dir(), &b_root).join("index.jsonl");
    let b_lines = std::fs::read_to_string(&b_index).unwrap_or_default();
    println!("  project b's history index: {b_lines:?}");
    assert!(
        !b_lines.contains(&format!("{}", f.dir.join("main.tex").display())),
        "project a's save went into project b's history"
    );
}

// ----- B-25: the root moves under a running agent session -----

fn regress2_root_move_keeps_the_agent_in_the_project() {
    common::require_agent_binaries();
    let base = common::temp_dir("root-move-agent");
    write_files(
        &base,
        &[
            ("start/readme.md", "r\n"),
            ("thesis/main.tex", "one\ntwo\n"),
        ],
    );
    let f = window_on(base.join("start"), |_| {});
    let agent = f.agent();
    agent.set_scenario(Some(scenario_file(
        &f,
        r#"{"default": [{"say": "hi"}],
            "fix": [{"disk_write": {"path": "main.tex", "content": "one\nTWO\n"}}]}"#,
    )));
    let events = f.events();
    // A question first: the agent starts in the window's root.
    agent.prompt(vec![PromptBlock::Text("hello".into())], PromptOrigin::Panel);
    let ended = |n: usize| {
        events
            .borrow()
            .iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnded(_)))
            .count()
            >= n
    };
    common::pump_until(AGENT_TIMEOUT, || ended(1));
    // Then the user opens the thesis: the root moves there.
    let thesis = base.join("thesis").canonicalize().unwrap();
    block_on(f.state.open_file(&thesis.join("main.tex"))).unwrap();
    assert_eq!(f.state.workspace.root().path(), thesis);
    agent.prompt(
        vec![PromptBlock::Text("fix main.tex".into())],
        PromptOrigin::Panel,
    );
    common::pump_until(AGENT_TIMEOUT, || ended(2));
    common::pump_for(Duration::from_millis(500));
    let doc = f.state.workspace.get(&thesis.join("main.tex")).unwrap();
    println!(
        "  stray file in the old root: {}; thesis hunks: {}",
        f.dir.join("main.tex").exists(),
        review_ui::hunk_count(&doc)
    );
    assert!(
        !f.dir.join("main.tex").exists(),
        "the agent still works in the old root: it wrote a main.tex the user cannot see"
    );
}

// ----- B-18: list_files on a large root -----

fn regress2_list_files_does_not_stall_the_main_thread() {
    let dir = common::temp_dir("list-big");
    for i in 0..5000 {
        let sub = dir.join(format!("d{}", i / 200));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(format!("f{i}.txt")), "").unwrap();
    }
    std::fs::write(dir.join("main.tex"), "\\documentclass{article}\n").unwrap();
    let f = window_on(dir, |_| {});
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    glib::spawn_future_local(tools::handle(
        f.state.clone(),
        ToolCall {
            call: 1,
            req: EditorRequest::ListFiles,
            reply: tx,
        },
    ));
    let context = glib::MainContext::default();
    let mut longest = Duration::ZERO;
    let deadline = Instant::now() + TIMEOUT;
    while rx.try_recv().is_err() {
        assert!(Instant::now() < deadline, "timed out");
        let started = Instant::now();
        context.iteration(false);
        longest = longest.max(started.elapsed());
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("  longest main-loop iteration during list_files: {longest:?}");
    assert!(
        longest <= Duration::from_millis(50),
        "list_files blocked the main thread for {longest:?}"
    );
}

// ----- B-28: an agent edit saved while the user's insert (a user action) is open -----

fn regress2_agent_save_inside_an_insert_keeps_modified_honest() {
    let f = fixture(
        "insert-save",
        &[("main.tex", "Let X be a scheme.\nWe type here\n")],
    );
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    let type_at_end_of_line_2 = |text: &str| {
        for c in text.chars() {
            let mut end = buffer.end_iter();
            end.backward_char();
            buffer.insert(&mut end, &c.to_string());
        }
    };
    // Vim's insert mode keeps one user action open until Esc.
    buffer.begin_user_action();
    type_at_end_of_line_2(" while Claude edits the lines abov");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    glib::spawn_future_local(tools::handle(
        f.state.clone(),
        ToolCall {
            call: 7,
            req: edit("main.tex", "Let X be a scheme.", "Let $X$ be a scheme."),
            reply: tx,
        },
    ));
    let context = glib::MainContext::default();
    let deadline = Instant::now() + TIMEOUT;
    while review_ui::hunk_count(&doc) == 0 {
        assert!(Instant::now() < deadline, "timed out");
        context.iteration(false);
    }
    // The user keeps typing while the edit is being saved.
    type_at_end_of_line_2("e");
    common::pump_until(TIMEOUT, || rx.try_recv().is_ok());
    type_at_end_of_line_2(" this one.");
    buffer.end_user_action();
    assert_eq!(
        f.disk("main.tex"),
        "Let $X$ be a scheme.\nWe type here while Claude edits the lines above\n"
    );
    buffer.undo();
    common::pump_for(Duration::from_millis(100));
    let text = doc.text();
    println!(
        "  after one undo: buffer={text:?} modified={} disk={:?}",
        doc.is_dirty(),
        f.disk("main.tex")
    );
    assert!(
        doc.is_dirty() || text == f.disk("main.tex"),
        "the buffer shows as saved but differs from the file"
    );
}

// ----- B-12: a large text file written during a turn -----

/// Runs the main loop until `done`, returning the longest single iteration.
fn longest_iteration_until(mut done: impl FnMut() -> bool) -> Duration {
    let context = glib::MainContext::default();
    let mut longest = Duration::ZERO;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        let started = Instant::now();
        context.iteration(false);
        longest = longest.max(started.elapsed());
        std::thread::sleep(Duration::from_millis(1));
    }
    longest
}

/// A script the agent runs writes a 25 MB `results.txt` (`.txt` is a source extension): the
/// file monitor reads, diffs and reviews all of it on the main thread, with no size cap.
fn regress2_big_file_written_in_a_turn_does_not_freeze() {
    let f = fixture("big-txt", &[("main.tex", "m\n")]);
    f.start_turn("go");
    let mut text = String::with_capacity(26 << 20);
    let mut i = 0u64;
    while text.len() < 25 << 20 {
        text.push_str(&format!(
            "{i} 0.{:08} 0.{:08}\n",
            i * 7919 % 100_000_000,
            i * 104_729 % 100_000_000
        ));
        i += 1;
    }
    let started = Instant::now();
    std::fs::write(f.path("results.txt"), &text).unwrap();
    // Handled when it is reviewed, or (a size cap) when the monitor is done with it.
    common::pump_for(Duration::from_millis(300));
    let longest =
        longest_iteration_until(|| hunks(&f, "results.txt") > 0 || watcher::is_idle(&f.state));
    println!(
        "  25 MB results.txt handled after {:.2?} (reviewed: {}); longest main-loop iteration \
         {longest:?}",
        started.elapsed(),
        hunks(&f, "results.txt") > 0
    );
    assert!(
        longest <= Duration::from_millis(100),
        "the window froze for {longest:?}"
    );
}

// ----- B-30: render_page opens a new poppler document for every call -----

/// Anonymous memory of this process in KiB, after giving freed memory back.
fn anon_kib() -> u64 {
    // Free heap pages go back to the system (malloc_trim on glibc).
    lemma_app::heap::trim();
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap();
    text.lines()
        .find(|l| l.starts_with("Anonymous:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// 40 render_page calls on a 5-page PDF (the agent checking its layout after each compile).
/// ARCHITECTURE §7 gives the agent inside the editor process 3 MB; the retained growth here is
/// tens of MiB (it plateaus after about 20 calls). Run it alone to keep other tests' memory out
/// of the measurement: `… --test gtk_regress2_b render_page_does_not_retain`.
fn regress2_render_page_does_not_retain_memory() {
    let dir = common::temp_dir("render-mem");
    write_files(
        &dir,
        &[(
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\nx\n\\end{document}\n",
        )],
    );
    std::fs::copy(
        common::fixtures().join("proj/main.pdf"),
        dir.join("main.pdf"),
    )
    .unwrap();
    let f = window_on(dir, |_| {});
    f.open("main.tex");
    // Every page once first (first-use caches).
    for page in 1..=5 {
        let reply = f.call(EditorRequest::RenderPage { page });
        assert!(!reply.is_error, "{}", tools::reply_text(&reply));
    }
    common::pump_for(Duration::from_millis(200));
    let base = anon_kib();
    for i in 0..40u32 {
        let reply = f.call(EditorRequest::RenderPage { page: i % 5 + 1 });
        assert!(!reply.is_error, "{}", tools::reply_text(&reply));
    }
    common::pump_for(Duration::from_millis(200));
    let grown = anon_kib().saturating_sub(base);
    println!("  anonymous memory after 40 more render_page calls: +{grown} KiB");
    assert!(grown <= 8 << 10, "render_page calls retained {grown} KiB");
}
