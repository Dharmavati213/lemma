//! WP2.4: the agent bridge (turn bookkeeping, commands, events), the MCP tool handlers, the file
//! monitor and local history. Tools are driven by injecting `ToolCall`s, the bridge by injecting
//! `AgentEvent`s (and a command channel); the `e2e_*` tests run the real chain (ACP client,
//! MCP server and bridge) with `lemma-fake-agent` and the scenarios in `tests/fixtures/agent`.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_agent_tools`.
//! Tests listed in [`IGNORED`] run only with `--ignored` or `--include-ignored`; the `e2e_*`
//! tests build `lemma-fake-agent` and `lemma-mcp` first (`cargo build --offline`).

mod common;

use adw::prelude::*;
use lemma_agent::{
    AgentCmd, AgentEvent, ConfigOpt, EditorRequest, PromptBlock, StopReason, ToolCall, ToolContent,
    ToolReply,
};
use lemma_app::agent::history::{self, HistoryReason};
use lemma_app::agent::{
    AgentController, AgentState, PromptOrigin, SentPrompt, bridge, tools, watcher,
};
use lemma_app::editor::Editor;
use lemma_app::review_ui::{self, Decision};
use lemma_app::testmode;
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
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);
const AGENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Tests that only run with `--ignored` or `--include-ignored`, with the reason (none at the
/// moment: the end-to-end tests build the fake agent and the MCP bridge themselves).
const IGNORED: &[(&str, &str)] = &[];

fn main() -> ExitCode {
    let tests: &[common::Test] = &[
        ("file_tools_reply_after_disk", file_tools),
        ("list_files_document_first_and_capped", list_files),
        ("outline_of_a_file_and_of_the_document", outline),
        ("compile_reports_errors_and_success", compile),
        ("compile_without_a_main_document", compile_without_root),
        ("diagnostics_of_the_last_build", diagnostics),
        ("selection_of_the_active_editor", selection),
        ("render_page_png", render_page),
        ("render_page_caps_tall_pages", render_page_tall),
        ("review_state_diff_and_events", review_state),
        ("tool_records_and_feedback", tool_records),
        ("turn_start_and_end_bookkeeping", turn_bookkeeping),
        (
            "turn_end_build_skipped_after_compile",
            turn_end_build_skipped,
        ),
        ("commands_and_events_through_a_channel", commands_and_events),
        ("start_failures_and_exits", start_failures),
        ("cancel_watchdog_ends_the_turn", cancel_watchdog),
        ("injected_chunks_main_thread_cost", injected_chunk_cost),
        ("watcher_turn_writes_become_hunks", watcher_turn),
        (
            "watcher_outside_a_turn_reloads_or_asks",
            watcher_outside_turn,
        ),
        ("watcher_ignores_our_own_writes", watcher_own_writes),
        (
            "watcher_outside_a_turn_with_reviews",
            watcher_outside_turn_reviews,
        ),
        (
            "watcher_git_baseline_for_files_not_snapshotted",
            watcher_git_baseline,
        ),
        ("history_on_save_and_turn_start", history_entries),
        ("history_restore_produces_hunks", history_restore),
        ("demo_agent_needs_a_scenario", demo_needs_scenario),
        ("e2e_basic_scenario_edits_two_files_and_builds", e2e_basic),
        ("e2e_bypass_scenario_becomes_hunks", e2e_bypass),
        ("e2e_errors_scenario_exact_texts", e2e_errors),
        ("e2e_long_scenario_chunk_cost", e2e_long),
        ("e2e_demo_agent_snapshot", e2e_demo),
        #[cfg(unix)]
        (
            "runtime_files_of_killed_runs_and_stopped_agents_go",
            runtime_files_cleaned_up,
        ),
    ];
    let args: Vec<String> = std::env::args().collect();
    let include_ignored = args
        .iter()
        .any(|a| a == "--ignored" || a == "--include-ignored");
    let only_ignored = args.iter().any(|a| a == "--ignored");
    let selected: Vec<common::Test> = tests
        .iter()
        .copied()
        .filter(|(name, _)| {
            let ignored = IGNORED.iter().find(|(n, _)| n == name);
            match ignored {
                Some((_, why)) if !include_ignored => {
                    println!("test {name} ... ignored, {why}");
                    false
                }
                None if only_ignored => false,
                _ => true,
            }
        })
        .collect();
    common::run(&selected)
}

// ----- fixtures -----

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

/// Waits for `f` with a timeout (panics after it).
fn block_on_timeout<F: Future>(timeout: Duration, f: F) -> F::Output {
    block_on(glib::future_with_timeout(timeout, f)).expect("timed out")
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
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
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    window_on(dir, configure)
}

/// A window on a copy of `tests/fixtures/<from>`.
fn fixture_copy(name: &str, from: &str, configure: impl FnOnce(&mut Config)) -> Fixture {
    let dir = common::temp_dir(name);
    copy_dir(&common::fixtures().join(from), &dir);
    window_on(dir, configure)
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

fn window_on(dir: PathBuf, configure: impl FnOnce(&mut Config)) -> Fixture {
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
    let home = PathBuf::from(format!("{}-home", dir.display()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // Unix socket paths are short: the runtime directory lives under $XDG_RUNTIME_DIR.
    let runtime = glib::user_runtime_dir().join(format!("lemma-t{}-{n}", std::process::id()));
    let (home_var, runtime_var) = (home.clone(), runtime.clone());
    let paths = Paths::from_vars(move |name| match name {
        "HOME" => Some(home_var.clone().into()),
        "XDG_RUNTIME_DIR" => Some(runtime_var.clone().into()),
        _ => None,
    })
    .unwrap();
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
        runtime,
    };
    // The file monitor watches the root before the test writes anything.
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
        let reply = rx
            .try_recv()
            .expect("exactly one reply, sent before the future resolved");
        assert!(rx.try_recv().is_err(), "a second reply");
        reply
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

    fn call_error(&self, req: EditorRequest) -> String {
        let reply = self.call(req);
        assert!(
            reply.is_error,
            "tool succeeded: {}",
            tools::reply_text(&reply)
        );
        tools::reply_text(&reply)
    }

    /// Writes `text` to `file` as another program would (a write in place).
    fn external_write(&self, file: &str, text: &str) {
        let path = self.path(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Writes `text` to `file` as `sed -i` does: a new file renamed over the old one.
    fn external_replace(&self, file: &str, text: &str) {
        let path = self.path(file);
        let tmp = path.with_file_name("sedXyZ12");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(tmp, path).unwrap();
    }

    fn wait_watcher(&self) {
        // Events arrive after the write; give them a moment, then wait for the batch.
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

    fn end_turn(&self, reason: StopReason) {
        bridge::handle_event(&self.agent(), AgentEvent::TurnEnded(reason));
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

fn ready_event(mode: &str) -> AgentEvent {
    AgentEvent::Ready {
        session: "s-1".into(),
        agent: "fake 1.0".into(),
        modes: vec![
            ("default".into(), "Default".into()),
            ("acceptEdits".into(), "Accept edits".into()),
            ("plan".into(), "Plan".into()),
        ],
        mode: mode.into(),
        options: vec![
            ConfigOpt {
                id: "model".into(),
                name: "Model".into(),
                current: "default".into(),
                values: vec![
                    ("default".into(), "Default".into()),
                    ("opus".into(), "Opus".into()),
                ],
            },
            ConfigOpt {
                id: "effort".into(),
                name: "Effort".into(),
                current: "high".into(),
                values: vec![("low".into(), "Low".into()), ("high".into(), "High".into())],
            },
        ],
        loadable: false,
    }
}

/// The commands sent so far.
fn drain(rx: &async_channel::Receiver<AgentCmd>) -> Vec<AgentCmd> {
    common::pump_for(Duration::from_millis(30));
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

// ----- tools -----

/// read_file, edit and write: replies after the change is on disk; the exact error texts.
fn file_tools() {
    let f = fixture(
        "file-tools",
        &[
            ("main.tex", "alpha\nbeta\ngamma\n"),
            ("ch.tex", "one\n"),
            ("twice.tex", "x and x\n"),
        ],
    );
    let editor = f.open("main.tex");
    let doc = editor.document().clone();

    let text = f.call_text(edit("main.tex", "beta", "BETA"));
    assert!(text.starts_with("Edited main.tex: line 2"), "{text}");
    assert_eq!(
        f.disk("main.tex"),
        doc.text(),
        "disk equals the buffer when the reply comes"
    );
    assert_eq!(review_ui::hunk_count(&doc), 1);
    // A closed file.
    f.call_text(edit("ch.tex", "one", "ONE"));
    assert_eq!(f.disk("ch.tex"), "ONE\n");
    assert!(!f.state.workspace.get(&f.path("ch.tex")).unwrap().is_open());

    // read_file returns the live buffer, unsaved typing included.
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.end_iter(), "delta\n");
    let read = f.call_text(EditorRequest::ReadFile {
        path: "main.tex".into(),
        offset: None,
        limit: None,
    });
    assert!(read.contains("     4\tdelta"), "{read}");

    // write creates a file (and its directory).
    let created = f.call_text(EditorRequest::Write {
        path: "sub/new.tex".into(),
        content: "\\section{New}\n".into(),
    });
    assert!(
        created.starts_with("Created sub/new.tex (1 line)"),
        "{created}"
    );
    assert_eq!(f.disk("sub/new.tex"), "\\section{New}\n");

    // The error texts of §4.2.
    let missing = f.call_error(edit("main.tex", "zzz", "y"));
    assert!(
        missing.starts_with(
            "old_string not found — the user may have edited this region; call read_file"
        ),
        "{missing}"
    );
    let twice = f.call_error(edit("twice.tex", "x", "y"));
    assert!(
        twice.starts_with("old_string matches 2 times; add context or set replace_all"),
        "{twice}"
    );
    let outside = f.call_error(EditorRequest::ReadFile {
        path: "../outside.tex".into(),
        offset: None,
        limit: None,
    });
    assert_eq!(outside, "path outside project root");
}

fn list_files() {
    let f = fixture(
        "list-files",
        &[
            (
                "main.tex",
                "\\documentclass{article}\n\\addbibresource{refs.bib}\n\\begin{document}\n\
                 \\input{chapters/a}\n\\end{document}\n",
            ),
            ("chapters/a.tex", "\\section{A}\n"),
            ("refs.bib", "@book{x, title={X}}\n"),
            ("notes.md", "notes\n"),
            ("figures/plot.png", "png"),
            ("main.aux", "\\relax\n"),
            ("main.synctex.gz", "x"),
            (".git/HEAD", "ref: refs/heads/main\n"),
            (".build/main.log", "log\n"),
            (".hidden.tex", "x\n"),
        ],
    );
    f.open("main.tex");
    let text = f.call_text(EditorRequest::ListFiles);
    let lines: Vec<&str> = text.lines().collect();
    let at = |s: &str| {
        lines
            .iter()
            .position(|l| *l == s)
            .unwrap_or_else(|| panic!("{s:?} not in\n{text}"))
    };
    assert!(at("main.tex (root document)") < at("chapters/a.tex"));
    assert!(at("chapters/a.tex") < at("refs.bib"));
    assert!(at("refs.bib") < at("Other files:"));
    assert!(at("Other files:") < at("figures/plot.png"));
    at("notes.md");
    for hidden in ["main.aux", "synctex", ".git", ".build", ".hidden"] {
        assert!(!text.contains(hidden), "{hidden} listed:\n{text}");
    }
    assert_eq!(
        lines
            .iter()
            .filter(|l| **l == "main.tex (root document)")
            .count()
            + lines.iter().filter(|l| **l == "main.tex").count(),
        1,
        "each file once"
    );

    // At most 500 entries.
    for i in 0..600 {
        f.external_write(&format!("many/f{i:03}.txt"), "x");
    }
    let text = f.call_text(EditorRequest::ListFiles);
    let listed = text
        .lines()
        .filter(|l| l.ends_with(".txt") || l.ends_with(".tex") || l.ends_with(".bib"))
        .count();
    assert!(listed <= tools::LIST_FILES_MAX, "{listed} entries");
    assert!(text.contains("the list stops at 500 entries"), "{text}");

    let empty = fixture("list-empty", &[]);
    assert!(
        empty
            .call_text(EditorRequest::ListFiles)
            .contains("The project has no files yet.")
    );
}

fn outline() {
    let f = fixture_copy("outline", "proj", |_| {});
    f.open("main.tex");
    let text = f.call_text(EditorRequest::Outline {
        path: Some("main.tex".into()),
    });
    assert!(
        text.contains("\n    39  \\section{Introduction}\n"),
        "{text}"
    );
    assert!(text.contains("\n    39    \\label{sec:intro}\n"), "{text}");
    assert!(text.contains("\\input{chapters/yoneda}"), "{text}");
    assert!(text.contains("\\begin{theorem}[fpqc descent]"), "{text}");
    assert!(text.contains("  \\subsection{Long computation}"), "{text}");
    assert!(!text.contains("chapters/yoneda.tex\n"), "one file only");

    // Without a path: the document from its root, includes followed.
    let whole = f.call_text(EditorRequest::Outline { path: None });
    assert!(
        whole.starts_with("Outline of the document main.tex"),
        "{whole}"
    );
    let main_at = whole.find("\nmain.tex\n").expect("main.tex");
    let chapter_at = whole
        .find("\nchapters/yoneda.tex\n")
        .expect("the included chapter");
    assert!(main_at < chapter_at);
    // From a chapter, the root is found through the build root or the chapter itself.
    assert_eq!(
        f.call_error(EditorRequest::Outline {
            path: Some("../x.tex".into())
        }),
        "path outside project root"
    );
    assert_eq!(
        f.call_error(EditorRequest::Outline {
            path: Some("missing.tex".into())
        }),
        "file does not exist: missing.tex"
    );
}

fn compile() {
    if !have_latexmk() {
        return;
    }
    let f = fixture_copy("compile-errors", "logs/undefcs", |_| {});
    f.open("main.tex");
    let text = f.call_text(EditorRequest::Compile);
    assert!(text.starts_with("Build failed with 1 error in "), "{text}");
    assert!(
        text.contains("\nmain.tex:66: Undefined control sequence.\n"),
        "{text}"
    );
    assert!(text.contains("main.pdf has "), "{text}");
    assert!(text.contains("Call get_diagnostics"), "{text}");

    let ok = fixture_copy("compile-ok", "proj", |_| {});
    ok.open("main.tex");
    let text = ok.call_text(EditorRequest::Compile);
    assert!(text.starts_with("Build succeeded in "), "{text}");
    assert!(text.contains("main.pdf has 5 pages."), "{text}");
    assert!(!text.contains("Errors:"), "{text}");
}

fn compile_without_root() {
    let f = fixture("compile-no-root", &[("notes.tex", "just some notes\n")]);
    f.open("notes.tex");
    let error = f.call_error(EditorRequest::Compile);
    assert!(error.starts_with("The build did not run: "), "{error}");
    assert!(error.contains("no main document"), "{error}");
}

fn diagnostics() {
    let none = fixture("diagnostics-none", &[("main.tex", "x\n")]);
    assert_eq!(
        none.call_text(EditorRequest::Diagnostics { path: None }),
        "No build has run in this session yet; call compile first."
    );
    if !have_latexmk() {
        return;
    }
    let f = fixture_copy("diagnostics", "logs/undefcs", |_| {});
    let editor = f.open("main.tex");
    f.call_text(EditorRequest::Compile);
    let all = f.call_text(EditorRequest::Diagnostics { path: None });
    assert!(all.starts_with("1 error, "), "counts first:\n{all}");
    assert!(
        all.contains("\nmain.tex:66: error: Undefined control sequence.\n"),
        "{all}"
    );
    assert!(all.contains(": warning: "), "{all}");
    // Lines follow the text: two lines inserted above the error move it.
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% one\n% two\n");
    common::pump_for(Duration::from_millis(100));
    let moved = f.call_text(EditorRequest::Diagnostics {
        path: Some("main.tex".into()),
    });
    assert!(
        moved.contains("\nmain.tex:68: error: Undefined control sequence.\n"),
        "{moved}"
    );
    let chapter = f.call_text(EditorRequest::Diagnostics {
        path: Some("chapters/yoneda.tex".into()),
    });
    assert!(!chapter.contains("main.tex:"), "{chapter}");
    assert_eq!(
        f.call_error(EditorRequest::Diagnostics {
            path: Some("/etc/passwd".into())
        }),
        "path outside project root"
    );
}

fn selection() {
    let f = fixture("selection", &[("main.tex", "alpha\nbeta gamma\n")]);
    assert_eq!(
        f.call_error(EditorRequest::Selection),
        "No file is open in the editor"
    );
    let editor = f.open("main.tex");
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line_offset(1, 2).unwrap());
    let text = f.call_text(EditorRequest::Selection);
    assert_eq!(
        text,
        "File: main.tex\nCursor: line 2, column 3\nNo text is selected.\n"
    );
    let start = buffer.iter_at_line_offset(1, 5).unwrap();
    let end = buffer.iter_at_line_offset(1, 10).unwrap();
    buffer.select_range(&end, &start);
    let text = f.call_text(EditorRequest::Selection);
    assert_eq!(
        text,
        "File: main.tex\nCursor: line 2, column 11\nSelected text (5 characters, from line 2, \
         column 6 to line 2, column 11):\ngamma\n"
    );
    // At most 20k characters.
    buffer.insert(&mut buffer.end_iter(), &"é".repeat(30_000));
    buffer.select_range(&buffer.end_iter(), &buffer.start_iter());
    let text = f.call_text(EditorRequest::Selection);
    assert!(text.contains("(30017 characters,"), "{}", &text[..200]);
    assert!(text.contains("[the selection is cut after 20000 characters"));
    let selected = text
        .split_once("):\n")
        .unwrap()
        .1
        .split("\n[the selection")
        .next()
        .unwrap();
    assert_eq!(selected.chars().count(), tools::SELECTION_MAX_CHARS);
}

fn png_size(bytes: &[u8]) -> (i32, i32) {
    let texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from(bytes)).expect("a valid PNG");
    (texture.width(), texture.height())
}

fn render_page() {
    let f = fixture_copy("render", "proj", |_| {});
    f.open("main.tex");
    let reply = f.call(EditorRequest::RenderPage { page: 2 });
    assert!(!reply.is_error, "{}", tools::reply_text(&reply));
    let ToolContent::Png(png) = &reply.content[0] else {
        panic!("an image first: {reply:?}");
    };
    let (w, h) = png_size(png);
    // Letter paper (612 × 792 pt) at 1.5×.
    assert_eq!((w, h), (918, 1188));
    let text = tools::reply_text(&reply);
    assert!(
        text.starts_with("Page 2 of 5 of main.pdf, rendered at 918×1188 pixels."),
        "{text}"
    );
    let copy = f.runtime.join("lemma/page-2.png");
    assert!(text.contains(&copy.display().to_string()), "{text}");
    assert_eq!(
        &std::fs::read(&copy).unwrap(),
        png,
        "the copy is the same image"
    );
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    let mode = std::fs::metadata(copy.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    #[cfg(unix)]
    assert_eq!(mode & 0o777, 0o700, "the runtime directory is private");

    assert_eq!(
        f.call_error(EditorRequest::RenderPage { page: 0 }),
        "Pages are numbered from 1"
    );
    let past = f.call_error(EditorRequest::RenderPage { page: 99 });
    assert_eq!(
        past,
        "main.pdf: page 99 does not exist: the PDF has 5 pages"
    );
    let none = fixture("render-none", &[("notes.tex", "x\n")]);
    none.open("notes.tex");
    assert_eq!(
        none.call_error(EditorRequest::RenderPage { page: 1 }),
        "There is no PDF yet; call compile first"
    );
}

/// A one-page PDF of `width` × `height` points with a black square.
fn tall_pdf(width: u32, height: u32) -> Vec<u8> {
    let content = "0 0 0 rg 10 10 100 100 re f\n";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Contents 4 0 R >>"),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn render_page_tall() {
    let f = fixture(
        "render-tall",
        &[(
            "main.tex",
            "\\documentclass{article}\n\\begin{document}\nx\n\\end{document}\n",
        )],
    );
    std::fs::write(f.path("main.pdf"), tall_pdf(600, 4000)).unwrap();
    f.open("main.tex");
    let reply = f.call(EditorRequest::RenderPage { page: 1 });
    assert!(!reply.is_error, "{}", tools::reply_text(&reply));
    let ToolContent::Png(png) = &reply.content[0] else {
        panic!("an image first");
    };
    let (w, h) = png_size(png);
    assert_eq!(h, tools::RENDER_MAX_HEIGHT, "scaled down to fit");
    assert_eq!(w, 240, "the aspect ratio is kept");
}

fn review_state() {
    let f = fixture("review-state", &[("main.tex", "a\nb\nc\n")]);
    let text = f.call_text(EditorRequest::ReviewState);
    assert!(
        text.starts_with("No pending changes: the user has accepted or rejected all of them.\n"),
        "{text}"
    );
    f.call_text(edit("main.tex", "b\n", "B\n"));
    let text = f.call_text(EditorRequest::ReviewState);
    assert!(
        text.starts_with("1 change awaiting review in 1 file."),
        "{text}"
    );
    assert!(text.contains("-b\n+B\n"), "{text}");
    assert!(text.contains("\nThe user has not accepted, rejected or edited anything"));

    let doc = f.state.workspace.get(&f.path("main.tex")).unwrap();
    let id = doc.review.borrow().as_ref().unwrap().hunks()[0].id;
    review_ui::decide(&f.state, &doc, id, Decision::Reject).unwrap();
    let text = f.call_text(EditorRequest::ReviewState);
    assert!(text.starts_with("No pending changes"), "{text}");
    assert!(
        text.contains("What the user did since the last report:\n<review_feedback>\n"),
        "{text}"
    );
    assert!(
        text.contains("main.tex: the user rejected line 2"),
        "{text}"
    );
    // Drained: reported once.
    assert!(
        !f.call_text(EditorRequest::ReviewState)
            .contains("<review_feedback>")
    );
}

/// The bridge keeps a record of every answered call, and every reply carries the feedback.
fn tool_records() {
    let f = fixture("records", &[("main.tex", "a\nb\n")]);
    let agent = f.agent();
    let seen: Rc<RefCell<Vec<bridge::ToolRecord>>> = Rc::default();
    let sink = seen.clone();
    let id = agent
        .bridge()
        .connect_tool_done(move |r| sink.borrow_mut().push(r.clone()));
    f.call_text(edit("main.tex", "a\n", "A\n"));
    let doc = f.state.workspace.get(&f.path("main.tex")).unwrap();
    review_ui::decide_document(&f.state, &doc, Decision::Accept);
    // The next reply of any tool carries the feedback.
    let listed = f.call_text(EditorRequest::ListFiles);
    assert!(
        listed.contains("<review_feedback>\nmain.tex: the user accepted line 1\n"),
        "{listed}"
    );
    let records = agent.bridge().tool_records();
    let tools: Vec<&str> = records.iter().map(|r| r.tool).collect();
    assert_eq!(tools, ["edit", "list_files"]);
    assert_eq!(records[0].path.as_deref(), Some(Path::new("main.tex")));
    assert!(!records[0].is_error);
    assert!(records[0].text.starts_with("Edited main.tex"));
    assert_eq!(seen.borrow().len(), 2);
    agent.bridge().disconnect_tool_done(id);
    f.call_text(EditorRequest::ListFiles);
    assert_eq!(seen.borrow().len(), 2);
    // A record keeps the start of a long reply (here a 2000-line read of a 300 KB file) and
    // its length.
    let line = format!("{}\n", "x".repeat(149));
    f.external_write("big.tex", &line.repeat(2000));
    let read = f.call_text(EditorRequest::ReadFile {
        path: "big.tex".into(),
        offset: None,
        limit: None,
    });
    assert!(read.len() > 300_000, "{}", read.len());
    let records = agent.bridge().tool_records();
    let last = records.last().unwrap();
    assert_eq!(last.tool, "read_file");
    assert!(
        last.text.len() <= bridge::TOOL_RECORD_TEXT,
        "{}",
        last.text.len()
    );
    assert!(read.starts_with(last.text.trim_end_matches('…')));
    assert!(last.text.ends_with('…'));
    assert_eq!(last.bytes, read.len());
    // Stopping the agent forgets them.
    agent.stop();
    assert!(agent.bridge().tool_records().is_empty());
}

// ----- the bridge -----

fn turn_bookkeeping() {
    let f = fixture(
        "turn",
        &[
            (
                "main.tex",
                "\\documentclass{article}\n\\begin{document}\nHello\n\\end{document}\n",
            ),
            ("ch.tex", "chapter\n"),
        ],
    );
    let agent = f.agent();
    let events = f.events();
    let prompts: Rc<RefCell<Vec<SentPrompt>>> = Rc::default();
    let sink = prompts.clone();
    agent.connect_prompt(move |p| sink.borrow_mut().push(p.clone()));
    let notified: Rc<RefCell<Vec<String>>> = Rc::default();
    let sink = notified.clone();
    f.state.set_notification_hook(Some(Box::new(move |id, _| {
        sink.borrow_mut().push(id.into())
    })));

    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.iter_at_line(2).unwrap(), "draft ");
    buffer.place_cursor(&buffer.iter_at_line_offset(2, 3).unwrap());
    assert!(doc.is_dirty());

    let (turn, blocks) = f.start_turn("fix it");
    assert_eq!(turn, 1);
    assert_eq!(agent.state(), AgentState::Running { turn: 1 });
    assert_eq!(agent.bridge().open_turn(), Some(1));
    assert!(!doc.is_dirty(), "saved before the prompt");
    assert!(f.disk("main.tex").contains("draft Hello"));
    // The snapshot holds the files that are not loaded.
    let snapshot = bridge::turn_snapshot(&agent).expect("a snapshot");
    assert_eq!(
        snapshot.files.get(&f.path("ch.tex")).map(String::as_str),
        Some("chapter\n")
    );
    assert!(!snapshot.files.contains_key(&f.path("main.tex")));
    assert!(snapshot.existed.contains(&f.path("main.tex")));
    assert!(snapshot.complete);
    // The user's text, then where the user is.
    assert_eq!(blocks[0], PromptBlock::Text("fix it".into()));
    let PromptBlock::Resource { uri, mime, text } = &blocks[1] else {
        panic!("a resource: {blocks:?}");
    };
    assert!(
        uri.starts_with("file:///") && uri.ends_with("/main.tex"),
        "{uri}"
    );
    assert_eq!(mime, "text/plain");
    assert!(
        text.starts_with(
            "The user's editor shows main.tex (4 lines); the cursor is on line 3, column 4.\n"
        ),
        "{text}"
    );
    assert!(text.contains("\n     3\tdraft Hello\n"), "{text}");
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        *prompts.borrow(),
        [SentPrompt {
            turn: 1,
            origin: PromptOrigin::Panel,
            blocks: vec![PromptBlock::Text("fix it".into())],
        }]
    );

    // The agent edits; the turn ends while the window is not focused.
    f.call_text(edit("main.tex", "Hello", "Bonjour"));
    f.state.set_focus_override(Some(false));
    f.end_turn(StopReason::EndTurn);
    assert_eq!(agent.state(), AgentState::Idle);
    assert_eq!(agent.bridge().open_turn(), None);
    assert_eq!(
        events.borrow().last(),
        Some(&AgentEvent::TurnEnded(StopReason::EndTurn))
    );
    assert_eq!(*notified.borrow(), [bridge::NOTIFICATION_ID]);
    assert_eq!(
        bridge::notification_text(&f.state, StopReason::EndTurn),
        (
            "Claude finished".to_owned(),
            "1 change to review".to_owned()
        )
    );
    let build = f.state.build.get().unwrap();
    assert!(
        build.is_busy(),
        "a file changed: the turn-end build was requested"
    );
    common::pump_until(BUILD_TIMEOUT, || !build.is_busy());
    assert!(bridge::in_agent_window(&agent), "within the grace period");

    // The next prompt starts with the review feedback.
    let id = doc.review.borrow().as_ref().unwrap().hunks()[0].id;
    review_ui::decide(&f.state, &doc, id, Decision::Reject).unwrap();
    f.state.set_focus_override(Some(true));
    let (turn, blocks) = f.start_turn("again");
    assert_eq!(turn, 2);
    let PromptBlock::Text(feedback) = &blocks[0] else {
        panic!("feedback first: {blocks:?}");
    };
    assert!(feedback.starts_with("<review_feedback>\nmain.tex: the user rejected line 3"));
    assert_eq!(blocks[1], PromptBlock::Text("again".into()));
    f.end_turn(StopReason::EndTurn);
    assert_eq!(notified.borrow().len(), 1, "focused: no notification");
    // A cancelled turn does not notify; nothing changed, so nothing is built.
    f.state.set_focus_override(Some(false));
    f.start_turn("third");
    f.end_turn(StopReason::Cancelled);
    assert_eq!(notified.borrow().len(), 1);
    assert!(!build.is_busy());
    f.state.set_notification_hook(None);
    f.state.set_focus_override(None);
    // The grace period is over: the snapshot goes.
    agent
        .bridge()
        .set_after_turn_grace(Duration::from_millis(50));
    f.start_turn("fourth");
    f.end_turn(StopReason::EndTurn);
    common::pump_until(TIMEOUT, || bridge::turn_snapshot(&agent).is_none());
    assert!(!bridge::in_agent_window(&agent));
}

fn turn_end_build_skipped() {
    let f = fixture("turn-build", &[("main.tex", "x\n")]);
    let agent = f.agent();
    let build = f.state.build.get().unwrap();
    f.start_turn("go");
    bridge::note_file_changed(&agent);
    bridge::note_compile(&agent);
    f.end_turn(StopReason::EndTurn);
    assert!(!build.is_busy(), "the agent's compile built everything");
    // A write of the same content changes nothing: no build.
    f.start_turn("go");
    let text = f.call_text(EditorRequest::Write {
        path: "main.tex".into(),
        content: "x\n".into(),
    });
    assert!(text.contains("already has this content"), "{text}");
    f.end_turn(StopReason::EndTurn);
    assert!(!build.is_busy());
    f.start_turn("go");
    bridge::note_compile(&agent);
    bridge::note_file_changed(&agent);
    f.end_turn(StopReason::EndTurn);
    assert!(build.is_busy(), "changed after the compile");
    common::pump_until(BUILD_TIMEOUT, || !build.is_busy());
    // build.on_agent_turn_end off: no build.
    let mut config = (*f.state.config.get()).clone();
    config.build.on_agent_turn_end = false;
    f.state.set_config(Rc::new(config));
    f.start_turn("go");
    bridge::note_file_changed(&agent);
    f.end_turn(StopReason::EndTurn);
    assert!(!build.is_busy());
}

fn commands_and_events() {
    let f = fixture_with("commands", &[("main.tex", "x\n")], |c| {
        c.agent.default_mode = "plan".into();
        c.agent.model = "opus".into();
        c.agent.effort = "max".into();
    });
    let agent = f.agent();
    let events = f.events();
    let (tx, rx) = async_channel::unbounded();
    bridge::attach_channel(&agent, tx);
    assert_eq!(agent.state(), AgentState::Starting);
    let started = agent.ensure_started();
    // Starting in acceptEdits (never offered): switched to the configured plan mode.
    bridge::handle_event(&agent, ready_event("acceptEdits"));
    assert_eq!(agent.state(), AgentState::Idle);
    assert_eq!(block_on_timeout(TIMEOUT, started), Ok(()));
    assert_eq!(
        drain(&rx),
        [
            AgentCmd::SetMode("plan".into()),
            AgentCmd::SetConfig {
                id: "model".into(),
                value: "opus".into()
            },
        ],
        "effort max is not offered: left alone"
    );
    assert!(matches!(events.borrow()[0], AgentEvent::Ready { .. }));

    agent.set_mode("acceptEdits");
    agent.set_mode("bypassPermissions");
    agent.set_mode("default");
    agent.set_config("effort", "low");
    agent.permission(7, Some("allow-once".into()));
    agent.permission(8, None);
    assert_eq!(
        drain(&rx),
        [
            AgentCmd::SetMode("default".into()),
            AgentCmd::SetConfig {
                id: "effort".into(),
                value: "low".into()
            },
            AgentCmd::Permission {
                request: 7,
                option_id: Some("allow-once".into())
            },
            AgentCmd::Permission {
                request: 8,
                option_id: None
            },
        ]
    );

    // A prompt goes out with the turn's blocks.
    f.open("main.tex");
    agent.prompt(vec![PromptBlock::Text("hi".into())], PromptOrigin::Panel);
    common::pump_until(TIMEOUT, || !rx.is_empty());
    let sent = drain(&rx);
    let [AgentCmd::Prompt(blocks)] = sent.as_slice() else {
        panic!("one prompt: {sent:?}");
    };
    assert_eq!(blocks[0], PromptBlock::Text("hi".into()));
    assert!(matches!(blocks[1], PromptBlock::Resource { .. }));
    assert_eq!(agent.state(), AgentState::Running { turn: 1 });
    // Refused while running.
    agent.prompt(vec![PromptBlock::Text("again".into())], PromptOrigin::Panel);
    assert!(matches!(
        events.borrow().last(),
        Some(AgentEvent::Error(m)) if m.starts_with("Claude is still working")
    ));
    assert!(drain(&rx).is_empty());
    // Cancel goes to the agent, which confirms.
    agent.cancel();
    assert_eq!(drain(&rx), [AgentCmd::Cancel]);
    bridge::handle_event(&agent, AgentEvent::TurnEnded(StopReason::Cancelled));
    assert_eq!(agent.state(), AgentState::Idle);
    // Events of every kind reach the callbacks in order.
    let before = events.borrow().len();
    for chunk in ["a", "b", "c"] {
        bridge::handle_event(&agent, AgentEvent::MessageChunk(chunk.into()));
    }
    bridge::handle_event(&agent, AgentEvent::Usage { used: 5, size: 10 });
    assert_eq!(
        events.borrow()[before..],
        [
            AgentEvent::MessageChunk("a".into()),
            AgentEvent::MessageChunk("b".into()),
            AgentEvent::MessageChunk("c".into()),
            AgentEvent::Usage { used: 5, size: 10 },
        ]
    );

    // The process dies during a turn: the turn ends, then Exited; a later start may retry.
    agent.prompt(vec![PromptBlock::Text("more".into())], PromptOrigin::Panel);
    common::pump_until(TIMEOUT, || agent.is_running() && !rx.is_empty());
    drain(&rx);
    let before = events.borrow().len();
    bridge::handle_event(&agent, AgentEvent::Exited(Some(1)));
    assert_eq!(
        events.borrow()[before..],
        [
            AgentEvent::TurnEnded(StopReason::Cancelled),
            AgentEvent::Exited(Some(1))
        ]
    );
    assert_eq!(
        agent.state(),
        AgentState::Failed("Claude exited unexpectedly (exit code 1)".into())
    );
    assert_eq!(drain(&rx), [AgentCmd::Shutdown]);
    assert!(rx.is_closed(), "the command channel is closed");
    agent.set_mode("plan");
    assert!(rx.try_recv().is_err(), "nothing goes to a dead agent");

    // Stop: Shutdown, Stopped; a stop during a turn ends it.
    let (tx, rx) = async_channel::unbounded();
    bridge::attach_channel(&agent, tx);
    bridge::handle_event(&agent, ready_event("plan"));
    drain(&rx);
    agent.prompt(vec![PromptBlock::Text("x".into())], PromptOrigin::Panel);
    common::pump_until(TIMEOUT, || agent.is_running() && !rx.is_empty());
    drain(&rx);
    let before = events.borrow().len();
    agent.stop();
    assert_eq!(agent.state(), AgentState::Stopped);
    assert_eq!(
        events.borrow()[before..],
        [AgentEvent::TurnEnded(StopReason::Cancelled)]
    );
    assert_eq!(drain(&rx), [AgentCmd::Shutdown]);
    assert!(!agent.bridge().has_session());
}

fn start_failures() {
    let f = fixture("start-failures", &[("main.tex", "x\n")]);
    let agent = f.agent();
    // An error before Ready fails the start for every caller.
    let (tx, _rx) = async_channel::unbounded();
    bridge::attach_channel(&agent, tx);
    let first = agent.ensure_started();
    let second = agent.ensure_started();
    bridge::handle_event(&agent, AgentEvent::Error("adapter missing".into()));
    assert_eq!(agent.state(), AgentState::Failed("adapter missing".into()));
    assert_eq!(
        block_on_timeout(TIMEOUT, first),
        Err("adapter missing".into())
    );
    assert_eq!(
        block_on_timeout(TIMEOUT, second),
        Err("adapter missing".into())
    );
    // An exit during the start.
    let (tx, _rx) = async_channel::unbounded();
    bridge::attach_channel(&agent, tx);
    let started = agent.ensure_started();
    bridge::handle_event(&agent, AgentEvent::Exited(Some(3)));
    let expected = "Claude exited while starting (exit code 3)".to_owned();
    assert_eq!(agent.state(), AgentState::Failed(expected.clone()));
    assert_eq!(block_on_timeout(TIMEOUT, started), Err(expected));
    // A stop while starting.
    let (tx, _rx) = async_channel::unbounded();
    bridge::attach_channel(&agent, tx);
    let started = agent.ensure_started();
    agent.stop();
    assert_eq!(
        block_on_timeout(TIMEOUT, started),
        Err("The agent was stopped".into())
    );
    // Starting for real without the fake agent's scenario file fails with a message, never
    // runs anything else.
    agent.set_scenario(Some(f.path("no-such-scenario.json")));
    let result = block_on_timeout(TIMEOUT, agent.ensure_started());
    let message = result.expect_err("cannot start");
    assert!(
        message.contains("no-such-scenario.json does not exist")
            || message.contains("lemma-fake-agent was not found")
            || message.contains("lemma-mcp was not found"),
        "{message}"
    );
    assert!(matches!(agent.state(), AgentState::Failed(_)));
}

fn cancel_watchdog() {
    let f = fixture("cancel", &[("main.tex", "x\n")]);
    let agent = f.agent();
    let events = f.events();
    let (tx, rx) = async_channel::unbounded();
    bridge::attach_channel(&agent, tx);
    bridge::handle_event(&agent, ready_event("default"));
    agent.prompt(vec![PromptBlock::Text("x".into())], PromptOrigin::Panel);
    common::pump_until(TIMEOUT, || agent.is_running() && !rx.is_empty());
    drain(&rx);
    let started = Instant::now();
    agent.cancel();
    assert_eq!(drain(&rx), [AgentCmd::Cancel]);
    // The agent never confirms: the turn ends here after the grace period.
    common::pump_until(TIMEOUT, || !agent.is_running());
    assert!(started.elapsed() >= bridge::CANCEL_GRACE - Duration::from_millis(50));
    assert_eq!(
        events.borrow().last(),
        Some(&AgentEvent::TurnEnded(StopReason::Cancelled))
    );
    // A late confirmation changes nothing.
    let count = events.borrow().len();
    bridge::handle_event(&agent, AgentEvent::TurnEnded(StopReason::Cancelled));
    assert_eq!(agent.state(), AgentState::Idle);
    assert_eq!(events.borrow().len(), count + 1);
}

/// Per-chunk main-thread time with everything that listens to the agent installed.
fn injected_chunk_cost() {
    let f = fixture("chunks", &[("main.tex", "x\n")]);
    let agent = f.agent();
    let (tx, _rx) = async_channel::unbounded();
    bridge::attach_channel(&agent, tx);
    bridge::handle_event(&agent, ready_event("default"));
    let mut times = Vec::with_capacity(1000);
    for i in 0..1000 {
        let started = Instant::now();
        bridge::handle_event(&agent, AgentEvent::MessageChunk(format!("word{i} ")));
        times.push(started.elapsed());
        if i % 50 == 0 {
            common::pump_for(Duration::from_millis(1));
        }
    }
    times.sort();
    let p99 = times[989];
    println!(
        "  per-chunk main-thread time: p50 {:?}, p99 {p99:?}",
        times[500]
    );
    assert!(p99 <= Duration::from_millis(2), "p99 {p99:?}");
}

// ----- the file monitor -----

fn watcher_turn() {
    let f = fixture(
        "watch-turn",
        &[
            ("main.tex", "one\ntwo\nthree\n"),
            ("chapters/a.tex", "alpha\nbeta\n"),
            ("b.tex", "bee\n"),
        ],
    );
    common::pump_until(TIMEOUT, || {
        watcher::watched_dirs(&f.state).contains(&f.path("chapters"))
    });
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    f.start_turn("go");

    f.external_write("main.tex", "one\nTWO\nthree\n");
    f.external_replace("chapters/a.tex", "alpha\nBETA\n");
    f.external_write("new.tex", "\\section{New}\n");
    std::fs::remove_file(f.path("b.tex")).unwrap();
    // A new directory with a file written before the directory could be watched.
    f.external_write("newdir/sub/x.tex", "x\n");
    common::pump_until(TIMEOUT, || {
        hunks(&f, "main.tex") == 1
            && hunks(&f, "chapters/a.tex") == 1
            && hunks(&f, "new.tex") == 1
            && hunks(&f, "b.tex") == 1
            && hunks(&f, "newdir/sub/x.tex") == 1
    });
    assert!(watcher::watched_dirs(&f.state).contains(&f.path("newdir/sub")));
    f.wait_watcher();
    // The open buffer took the disk's text as an agent change.
    assert_eq!(doc.text(), "one\nTWO\nthree\n");
    assert!(!doc.is_dirty());
    assert_eq!(
        baseline(&f, "main.tex").as_deref(),
        Some("one\ntwo\nthree\n")
    );
    // A file that was not loaded is reviewed against the snapshot.
    let a = f.state.workspace.get(&f.path("chapters/a.tex")).unwrap();
    assert!(!a.is_open());
    assert_eq!(a.text(), "alpha\nBETA\n");
    assert_eq!(
        baseline(&f, "chapters/a.tex").as_deref(),
        Some("alpha\nbeta\n")
    );
    // A new file is a new file; a deleted one can be restored.
    let new = f.state.workspace.get(&f.path("new.tex")).unwrap();
    assert!(review_ui::is_new_file(&new));
    assert_eq!(baseline(&f, "new.tex").as_deref(), Some(""));
    let b = f.state.workspace.get(&f.path("b.tex")).unwrap();
    assert_eq!(b.text(), "");
    review_ui::decide_document(&f.state, &b, Decision::Reject);
    // (GIO creates a missing file in place: it exists before it has its content.)
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(f.path("b.tex")).is_ok_and(|t| t == "bee\n")
    });
    // Saving the reviewed documents never meets an "externally modified" refusal.
    assert_eq!(block_on(a.save()), Ok(()));

    // The turn's end: changed files trigger the build.
    let build = f.state.build.get().unwrap();
    f.end_turn(StopReason::EndTurn);
    assert!(build.is_busy());
    common::pump_until(BUILD_TIMEOUT, || !build.is_busy());
    // Within the grace period after the turn, changes still count as the agent's.
    f.external_write("main.tex", "one\nTWO\nTHREE\n");
    common::pump_until(TIMEOUT, || doc.text() == "one\nTWO\nTHREE\n");
    assert_eq!(
        review_ui::hunk_count(&doc),
        1,
        "merged into the agent's hunk"
    );
    assert_eq!(
        doc.review.borrow().as_ref().unwrap().hunks()[0].cur_lines,
        1..3
    );
}

fn watcher_outside_turn() {
    let f = fixture(
        "watch-outside",
        &[
            ("clean.tex", "c1\n"),
            ("dirty.tex", "d1\n"),
            ("gone.tex", "g\n"),
        ],
    );
    let clean = f.open("clean.tex").document().clone();
    let dirty_editor = f.open("dirty.tex");
    let dirty = dirty_editor.document().clone();
    let gone = f.open("gone.tex").document().clone();
    let buffer = dirty_editor.buffer();
    buffer.insert(&mut buffer.end_iter(), "mine\n");

    // A clean buffer reloads (no review: not the agent's).
    f.external_write("clean.tex", "c1 changed\n");
    common::pump_until(TIMEOUT, || clean.text() == "c1 changed\n");
    assert!(!clean.is_dirty());
    assert!(clean.review.borrow().is_none());
    // A dirty one asks.
    f.external_write("dirty.tex", "d1 other\n");
    common::pump_until(TIMEOUT, || watcher::conflict_bar_shown(&f.state));
    assert_eq!(watcher::conflicts(&f.state), [f.path("dirty.tex")]);
    assert_eq!(dirty.text(), "d1\nmine\n", "the user's text stays");
    // Keep mine: the next save overwrites without asking.
    block_on(watcher::resolve_conflict(
        &f.state,
        &f.path("dirty.tex"),
        watcher::Resolution::KeepMine,
    ))
    .unwrap();
    assert!(!watcher::conflict_bar_shown(&f.state));
    assert!(dirty.is_dirty());
    assert_eq!(block_on(dirty.save()), Ok(()));
    assert_eq!(f.disk("dirty.tex"), "d1\nmine\n");
    f.wait_watcher();
    // Reload: the file's text.
    buffer.insert(&mut buffer.end_iter(), "more\n");
    f.external_write("dirty.tex", "d2\n");
    common::pump_until(TIMEOUT, || watcher::conflict_bar_shown(&f.state));
    block_on(watcher::resolve_conflict(
        &f.state,
        &f.path("dirty.tex"),
        watcher::Resolution::Reload,
    ))
    .unwrap();
    assert_eq!(dirty.text(), "d2\n");
    assert!(!dirty.is_dirty());
    assert!(watcher::conflicts(&f.state).is_empty());
    // Saving the user's own changes to a dirty document resolves its conflict too.
    buffer.insert(&mut buffer.end_iter(), "x\n");
    f.external_write("dirty.tex", "d3\n");
    common::pump_until(TIMEOUT, || watcher::conflict_bar_shown(&f.state));
    assert_eq!(block_on(dirty.save_overwriting()), Ok(()));
    common::pump_until(TIMEOUT, || !watcher::conflict_bar_shown(&f.state));
    // A deleted file: the buffer is kept and marked modified; saving writes it again.
    std::fs::remove_file(f.path("gone.tex")).unwrap();
    common::pump_until(TIMEOUT, || gone.is_dirty());
    assert_eq!(gone.text(), "g\n");
    assert_eq!(block_on(gone.save()), Ok(()));
    assert_eq!(f.disk("gone.tex"), "g\n");
    // A touch (same content) changes nothing.
    let version = clean.version();
    f.external_write("clean.tex", "c1 changed\n");
    f.wait_watcher();
    assert_eq!(clean.version(), version);
    // Files that are not loaded are left alone outside a turn.
    f.external_write("other.tex", "o\n");
    f.wait_watcher();
    assert!(f.state.workspace.get(&f.path("other.tex")).is_none());
}

/// Outside a turn, another program's change never becomes agent hunks of a decided review;
/// with hunks pending, the review shows it against the accepted text.
fn watcher_outside_turn_reviews() {
    let f = fixture(
        "watch-reviews",
        &[
            ("open.tex", "o1\no2\n"),
            ("closed.tex", "c1\nc2\n"),
            ("pending.tex", "p1\np2\np3\n"),
        ],
    );
    let open = f.open("open.tex").document().clone();
    let pending = f.open("pending.tex").document().clone();
    f.call_text(edit("open.tex", "o1", "O1"));
    f.call_text(edit("closed.tex", "c1", "C1"));
    f.call_text(edit("pending.tex", "p1", "P1"));
    let closed = f.state.workspace.get(&f.path("closed.tex")).unwrap();
    review_ui::decide_document(&f.state, &open, Decision::Accept);
    review_ui::decide_document(&f.state, &closed, Decision::Accept);
    f.wait_watcher();

    f.external_write("open.tex", "O1\no2 changed\n");
    f.external_write("closed.tex", "C1\nc2 changed\n");
    f.external_write("pending.tex", "P1\np2\np3 changed\n");
    common::pump_until(TIMEOUT, || {
        open.text() == "O1\no2 changed\n" && pending.text() == "P1\np2\np3 changed\n"
    });
    common::pump_until(TIMEOUT, || {
        f.state.workspace.get(&f.path("closed.tex")).is_none()
    });
    f.wait_watcher();
    assert!(open.review.borrow().is_none(), "the decided review went");
    assert!(!open.is_dirty());
    assert_eq!(
        review_ui::hunk_count(&pending),
        2,
        "the pending review shows both"
    );
    assert_eq!(baseline(&f, "pending.tex").as_deref(), Some("p1\np2\np3\n"));
    // The closed document is read again when it is needed.
    let read = f.call_text(EditorRequest::ReadFile {
        path: "closed.tex".into(),
        offset: None,
        limit: None,
    });
    assert!(read.contains("c2 changed"), "{read}");
}

fn watcher_own_writes() {
    let f = fixture("watch-own", &[("main.tex", "a\nb\n"), ("ch.tex", "c\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    f.start_turn("go");
    f.call_text(edit("main.tex", "a\n", "A\n"));
    f.call_text(edit("ch.tex", "c\n", "C\n"));
    let version = doc.version();
    let ch = f.state.workspace.get(&f.path("ch.tex")).unwrap();
    let ch_version = ch.version();
    common::pump_for(Duration::from_millis(500));
    f.wait_watcher();
    assert_eq!(doc.version(), version, "our own write is not applied again");
    assert_eq!(ch.version(), ch_version);
    assert_eq!(review_ui::hunk_count(&doc), 1);
    f.end_turn(StopReason::EndTurn);
    // A user save outside the turn.
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.end_iter(), "typed\n");
    assert_eq!(block_on(doc.save()), Ok(()));
    let version = doc.version();
    common::pump_for(Duration::from_millis(500));
    f.wait_watcher();
    assert_eq!(doc.version(), version);
    assert!(watcher::conflicts(&f.state).is_empty());
    assert!(!doc.is_dirty());
    // Local history's writes are not in the project.
    history::write_waiting(&f.state);
    block_on(history::flush(&f.state));
    f.wait_watcher();
    assert_eq!(doc.version(), version);
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Lemma Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A file whose text is not in the snapshot (over the cap) gets git's `HEAD` text as its
/// baseline; one git does not track is left alone (a toast says so).
fn watcher_git_baseline() {
    let f = fixture_with(
        "watch-git",
        &[("tracked.tex", "base\n"), ("untracked.tex", "u\n")],
        |c| c.agent.snapshot_cap_mib = 0,
    );
    // A throwaway repository of the test's own (no global configuration, no signing).
    if !(git(&f.dir, &["init", "-q"])
        && git(&f.dir, &["add", "tracked.tex"])
        && git(&f.dir, &["commit", "-q", "-m", "init"]))
    {
        println!("  SKIP: git is not usable");
        return;
    }
    f.wait_watcher();
    f.start_turn("go");
    let snapshot = bridge::turn_snapshot(&f.agent()).unwrap();
    assert!(snapshot.files.is_empty(), "the cap keeps no text");
    f.external_write("tracked.tex", "changed\n");
    f.external_write("untracked.tex", "u2\n");
    common::pump_until(TIMEOUT, || hunks(&f, "tracked.tex") == 1);
    f.wait_watcher();
    assert_eq!(baseline(&f, "tracked.tex").as_deref(), Some("base\n"));
    assert!(f.state.workspace.get(&f.path("untracked.tex")).is_none());
}

// ----- local history -----

fn history_dir(f: &Fixture) -> PathBuf {
    let paths = f.state.paths.clone().unwrap();
    history::project_dir(&paths.history_dir(), &f.dir)
}

fn history_entries() {
    let f = fixture("history", &[("main.tex", "v1\n"), ("ch.tex", "c1\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    let set = |text: &str| {
        buffer.set_text(text);
        assert_eq!(block_on(doc.save()), Ok(()));
    };
    set("v2\n");
    block_on(history::flush(&f.state));
    let entries = block_on(history::entries(&f.state, &f.path("main.tex")));
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].reason, HistoryReason::Save);
    assert_eq!(entries[0].path, f.path("main.tex"));
    assert_eq!(
        block_on(history::load(&f.state, &entries[0])).unwrap(),
        "v2\n"
    );
    // Saves within a minute are coalesced: the newest text is written when it is over.
    set("v3\n");
    set("v4\n");
    block_on(history::flush(&f.state));
    assert_eq!(
        block_on(history::entries(&f.state, &f.path("main.tex"))).len(),
        1
    );
    history::write_waiting(&f.state);
    block_on(history::flush(&f.state));
    let entries = block_on(history::entries(&f.state, &f.path("main.tex")));
    assert_eq!(entries.len(), 2);
    assert_eq!(
        block_on(history::load(&f.state, &entries[0])).unwrap(),
        "v4\n"
    );
    // The same content again: no entry.
    assert_eq!(block_on(doc.save()), Ok(()));
    history::write_waiting(&f.state);
    block_on(history::flush(&f.state));
    assert_eq!(
        block_on(history::entries(&f.state, &f.path("main.tex"))).len(),
        2
    );

    // A turn start records the files that changed since their last entry.
    f.start_turn("go");
    block_on(history::flush(&f.state));
    assert_eq!(
        block_on(history::entries(&f.state, &f.path("main.tex"))).len(),
        2,
        "main.tex did not change since its last entry"
    );
    let ch = block_on(history::entries(&f.state, &f.path("ch.tex")));
    assert_eq!(ch.len(), 1);
    assert_eq!(ch[0].reason, HistoryReason::Turn(1));
    assert_eq!(block_on(history::load(&f.state, &ch[0])).unwrap(), "c1\n");
    f.end_turn(StopReason::EndTurn);

    // The store on disk.
    let dir = history_dir(&f);
    let index = std::fs::read_to_string(dir.join("index.jsonl")).unwrap();
    assert_eq!(index.lines().count(), 3);
    assert!(index.contains("\"reason\":\"turn:1\""), "{index}");
    assert!(index.contains("\"reason\":\"save\""), "{index}");
    assert_eq!(std::fs::read_dir(dir.join("objects")).unwrap().count(), 3);

    // Off: nothing is written.
    let mut config = (*f.state.config.get()).clone();
    config.history.enabled = false;
    f.state.set_config(Rc::new(config));
    set("v5\n");
    history::write_waiting(&f.state);
    block_on(history::flush(&f.state));
    assert_eq!(
        std::fs::read_to_string(dir.join("index.jsonl")).unwrap(),
        index
    );

    // The menu item and its action.
    assert!(f.state.window.lookup_action("history-restore").is_some());
    let menu = &f.state.header.menu_files;
    let found = (0..menu.n_items()).any(|i| {
        menu.item_attribute_value(i, "action", Some(glib::VariantTy::STRING))
            .and_then(|v| v.get::<String>())
            .as_deref()
            == Some("win.history-restore")
    });
    assert!(found, "Restore from History… is in the file menu");
}

fn history_restore() {
    let f = fixture("history-restore", &[("main.tex", "first\nsecond\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    // An old version, then the user rewrites the file.
    assert_eq!(block_on(doc.save()), Ok(()));
    history::write_waiting(&f.state);
    block_on(history::flush(&f.state));
    buffer.set_text("first\nchanged\nthird\n");
    assert_eq!(block_on(doc.save()), Ok(()));
    history::write_waiting(&f.state);
    block_on(history::flush(&f.state));
    let entries = block_on(history::entries(&f.state, &f.path("main.tex")));
    assert_eq!(entries.len(), 2);
    let old = entries.last().unwrap();
    let count = block_on(history::restore(&f.state, &doc, old)).unwrap();
    assert!(count >= 1, "reviewable hunks");
    assert_eq!(doc.text(), "first\nsecond\n");
    assert_eq!(f.disk("main.tex"), "first\nsecond\n");
    assert_eq!(
        baseline(&f, "main.tex").as_deref(),
        Some("first\nchanged\nthird\n")
    );
    // Rejecting gives the current version back.
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    assert_eq!(doc.text(), "first\nchanged\nthird\n");
    // The dialog lists the entries (scripted runs do not block on it).
    WidgetExt::activate_action(&f.state.window, "win.history-restore", None).unwrap();
    common::pump_until(TIMEOUT, || f.state.window.visible_dialog().is_some());
    let dialog = f.state.window.visible_dialog().unwrap();
    assert_eq!(dialog.title(), "Restore main.tex");
    dialog.close();
}

// ----- the demo -----

fn demo_needs_scenario() {
    let dir = common::temp_dir("demo-no-scenario");
    std::fs::write(dir.join("main.tex"), "x\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lemma"))
        .arg(dir.join("main.tex"))
        .args(["--demo", "agent", "--quit-after-ms", "300"])
        .env("LEMMA_CONFIG", "/dev/null")
        .output()
        .expect("run lemma");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("--demo agent needs --agent-scenario"),
        "{stderr}"
    );
}

// ----- end to end with lemma-fake-agent and lemma-mcp -----

fn scenario(name: &str) -> PathBuf {
    let path = common::fixtures().join("agent").join(name);
    assert!(path.is_file(), "missing scenario {}", path.display());
    path.canonicalize().unwrap()
}

/// Builds `lemma-fake-agent` and `lemma-mcp` next to this test (see
/// [`common::require_agent_binaries`]).
fn require_binaries() {
    common::require_agent_binaries();
}

/// Runs one prompt of `scenario` on a copy of the `proj` fixture; returns the fixture and the
/// events of the turn.
fn run_scenario(name: &str, file: &str, prompt: &str) -> (Fixture, Rc<RefCell<Vec<AgentEvent>>>) {
    require_binaries();
    let f = fixture_copy(&format!("e2e-{file}"), "proj", |_| {});
    let agent = f.agent();
    agent.set_scenario(Some(scenario(file)));
    f.open("main.tex");
    let events = f.events();
    assert_eq!(
        block_on_timeout(AGENT_TIMEOUT, agent.ensure_started()),
        Ok(())
    );
    assert_eq!(agent.state(), AgentState::Idle, "{name}");
    agent.prompt(vec![PromptBlock::Text(prompt.into())], PromptOrigin::Panel);
    common::pump_until(AGENT_TIMEOUT, || {
        events
            .borrow()
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnEnded(_)))
    });
    (f, events)
}

fn e2e_basic() {
    let (f, events) = run_scenario("basic", "basic.json", "please edit");
    assert!(
        events
            .borrow()
            .contains(&AgentEvent::TurnEnded(StopReason::EndTurn)),
        "{:?}",
        events.borrow()
    );
    let reviewed = review_ui::reviewed_documents(&f.state.workspace);
    let names: Vec<String> = reviewed
        .iter()
        .map(|d| review_ui::relative_name(&f.state, d.path()))
        .collect();
    assert_eq!(names, ["chapters/yoneda.tex", "main.tex"]);
    for doc in &reviewed {
        assert_eq!(
            std::fs::read_to_string(doc.path()).unwrap(),
            doc.text(),
            "disk holds the proposal"
        );
    }
    let changes = f
        .state
        .review
        .get()
        .and_then(|r| r.changes_panel())
        .expect("the Changes panel");
    common::pump_until(TIMEOUT, || changes.rows().len() == 2);
    let build = f.state.build.get().unwrap();
    common::pump_until(BUILD_TIMEOUT, || !build.is_busy());
    assert!(build.last_outcome().is_some(), "the build ran");
    let records = f.agent().bridge().tool_records();
    assert!(records.iter().any(|r| r.tool == "compile"), "{records:?}");
    f.agent().stop();
    assert_eq!(f.agent().state(), AgentState::Stopped);
}

fn e2e_bypass() {
    let (f, _events) = run_scenario("bypass", "bypass.json", "please edit");
    common::pump_until(TIMEOUT, || review_ui::summary(&f.state.workspace).1 > 0);
    let records = f.agent().bridge().tool_records();
    assert!(
        !records
            .iter()
            .any(|r| r.tool == "edit" || r.tool == "write"),
        "the change came through the file monitor: {records:?}"
    );
}

fn e2e_errors() {
    let (f, _events) = run_scenario("errors", "errors.json", "please edit");
    let records = f.agent().bridge().tool_records();
    let errors: Vec<&str> = records
        .iter()
        .filter(|r| r.is_error)
        .map(|r| r.text.as_str())
        .collect();
    let has = |prefix: &str| errors.iter().any(|t| t.starts_with(prefix));
    assert!(
        has("old_string not found — the user may have edited this region; call read_file"),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|t| t.starts_with("old_string matches ")
            && t.contains(" times; add context or set replace_all")),
        "{errors:?}"
    );
    assert!(errors.contains(&"path outside project root"), "{errors:?}");
}

fn e2e_long() {
    let (f, events) = run_scenario("long", "long.json", "please edit");
    let chunks = events
        .borrow()
        .iter()
        .filter(|e| matches!(e, AgentEvent::MessageChunk(_)))
        .count();
    assert!(chunks >= 200, "{chunks} chunks");
    let durations = f.agent().bridge().event_durations();
    let mut times: Vec<Duration> = durations
        .iter()
        .filter(|(kind, _)| *kind == "message_chunk")
        .map(|(_, t)| *t)
        .collect();
    assert!(times.len() >= 200, "{} chunks timed", times.len());
    times.sort();
    let p99 = times[times.len() * 99 / 100];
    let slowest = durations.iter().max_by_key(|(_, t)| *t).unwrap();
    println!(
        "  {} events; per-chunk p99 {p99:?}; slowest event: {} {:?}",
        durations.len(),
        slowest.0,
        slowest.1
    );
    assert!(p99 <= Duration::from_millis(2), "p99 {p99:?}");
    // The 30 edits all landed.
    let doc = f
        .state
        .workspace
        .get(&f.path("long-scenario.tex"))
        .expect("the scenario's file");
    let edits = f
        .agent()
        .bridge()
        .tool_records()
        .iter()
        .filter(|r| r.tool == "edit" && !r.is_error)
        .count();
    assert_eq!(edits, 30);
    assert!(review_ui::hunk_count(&doc) >= 1);
}

fn e2e_demo() {
    require_binaries();
    let dir = common::temp_dir("e2e-demo");
    copy_dir(&common::fixtures().join("proj"), &dir);
    let png = dir.join("demo.png");
    let mut command = Command::new(env!("CARGO_BIN_EXE_lemma"));
    command
        .arg(dir.join("main.tex"))
        .args(["--demo", "agent", "--agent-scenario"])
        .arg(scenario("basic.json"))
        .arg("--snapshot")
        .arg(&png)
        .args(["--quit-after-ms", "1000"])
        .env("LEMMA_CONFIG", "/dev/null");
    if let Some(fake) = testmode::fake_agent_binary() {
        command.env(testmode::FAKE_AGENT_ENV, fake);
    }
    let out = command.output().expect("run lemma");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{:?}", out.status);
    assert!(png.is_file());
}

/// The MCP sockets of runs that were killed (their process is gone) are removed when an agent
/// starts; those of live processes stay. The agent's own socket and the `render_page` copies go
/// when it stops, and when its window leaves the application (the app quitting).
#[cfg(unix)]
fn runtime_files_cleaned_up() {
    use std::os::unix::net::UnixListener;
    require_binaries();
    let f = fixture_copy("runtime-files", "proj", |_| {});
    let runtime = f.runtime.join("lemma");
    std::fs::create_dir_all(&runtime).unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let stale = runtime.join(format!("mcp-{dead}-1.sock"));
    drop(UnixListener::bind(&stale).unwrap());
    // A socket of a live process (here: this one, under a number no session of ours uses).
    let live = runtime.join(format!("mcp-{}-999999.sock", std::process::id()));
    drop(UnixListener::bind(&live).unwrap());
    let ours = || -> Vec<PathBuf> {
        let prefix = format!("mcp-{}-", std::process::id());
        let mut found: Vec<PathBuf> = std::fs::read_dir(&runtime)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p != &live
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
            })
            .collect();
        found.sort();
        found
    };
    let agent = f.agent();
    agent.set_scenario(Some(scenario("basic.json")));
    f.open("main.tex");
    assert_eq!(
        block_on_timeout(AGENT_TIMEOUT, agent.ensure_started()),
        Ok(())
    );
    assert!(!stale.exists(), "the killed run's socket was removed");
    assert!(live.exists(), "a live process's socket stays");
    let socket = ours();
    assert_eq!(socket.len(), 1, "{socket:?}");
    let reply = f.call(EditorRequest::RenderPage { page: 1 });
    assert!(!reply.is_error, "{}", tools::reply_text(&reply));
    let copy = runtime.join("page-1.png");
    assert!(copy.exists());
    agent.stop();
    assert!(!socket[0].exists(), "Stop removes the agent's socket");
    assert!(!copy.exists(), "Stop removes the render_page copies");
    assert!(live.exists());
    // Started again, then the window leaves the application.
    assert_eq!(
        block_on_timeout(AGENT_TIMEOUT, agent.ensure_started()),
        Ok(())
    );
    let socket = ours();
    assert_eq!(socket.len(), 1, "{socket:?}");
    f.call(EditorRequest::RenderPage { page: 2 });
    let copy = runtime.join("page-2.png");
    assert!(copy.exists());
    f.state.app.remove_window(&f.state.window);
    common::pump_for(Duration::from_millis(50));
    assert_eq!(agent.state(), AgentState::Stopped);
    assert!(!socket[0].exists(), "quitting removes the agent's socket");
    assert!(!copy.exists(), "quitting removes the render_page copies");
    assert!(live.exists());
}
