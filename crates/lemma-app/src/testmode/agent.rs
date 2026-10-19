//! `--demo agent` (WP2.4/WP2.5): one agent turn against the scripted test agent.
//!
//! [`run_demo`] runs after FILE is open (usually `tests/fixtures/proj/main.tex` copied to a
//! temporary directory, with `--agent-scenario tests/fixtures/agent/basic.json`, see
//! `AgentController::scenario`): it opens the agent pane, sends [`DEMO_PROMPT`], and resolves
//! once the UI shows what a snapshot should capture: the turn has ended (or is waiting on a
//! permission card), changes other programs made during the turn have reached the file monitor,
//! the build the turn triggered has finished, and the editor shows the first hunk.
//! Errors make the run exit with status 1.
//!
//! The demo needs `--agent-scenario`: it never starts the real agent (that would cost money and
//! send the document to a model). The one exception is [`REAL_ENV`] (`LEMMA_DEMO_REAL_AGENT=1`),
//! for the maintainer's deliberate checks against the real adapter: without a scenario the demo
//! then starts the configured Claude agent. [`HANDSHAKE_ENV`] (`LEMMA_DEMO_HANDSHAKE_ONLY=1`)
//! stops right after `initialize` + `session/new` (no prompt, no model call) and prints the
//! `Ready` details. With a real agent the report is followed by one `agent-demo-tool:` line per
//! tool call (title and kind) and an `agent-demo-message:` line with the agent's reply.
//!
//! **End-to-end knobs** (WP2.11, `scripts/e2e-agent.sh`, `scripts/shots.sh`,
//! `tests/gtk_e2e_agent.rs`), read from the environment so the user-facing command line stays
//! small:
//!
//! - [`PROMPT_ENV`] (`LEMMA_DEMO_PROMPT`): the prompt to send instead of [`DEMO_PROMPT`] (a
//!   scenario keyword such as `reject` picks that scenario's steps);
//! - [`ANSWER_ENV`] (`LEMMA_DEMO_ANSWER`): `y`, `Y` or `n` answers every permission card with
//!   that key, as the user would (the card gets the focus through `win.agent-focus`, then the
//!   panel's key handler runs), and the demo waits for the turn's end. Unset, the demo stops at
//!   the first card so the snapshot shows it;
//! - [`CANCEL_ENV`] (`LEMMA_DEMO_CANCEL_MS`): activates `win.agent-stop` (the Stop button,
//!   Ctrl+Shift+Backspace) that many milliseconds after the prompt was sent;
//! - [`INLINE_ENV`] (`LEMMA_DEMO_INLINE`): sends this instruction through the Ctrl+K inline
//!   edit (`win.agent-inline-edit`, then Enter in its entry) instead of the panel, on the
//!   paragraph at the cursor; [`LINE_ENV`] (`LEMMA_DEMO_LINE`) puts the cursor on that 1-based
//!   line first. The agent pane stays closed;
//! - [`RETURN_ENV`] (`LEMMA_DEMO_RETURN_MS`): resolves that many milliseconds after the prompt
//!   was sent even when the turn still runs (a snapshot of a running turn).
//!
//! At the end the demo prints one line on stdout, [`REPORT_PREFIX`] followed by
//! `key=value` fields separated by spaces: `stop` (the stop reason, `permission` when the run
//! stopped at a card, `running` when [`RETURN_ENV`] cut it short), `turn`, `chunks` (agent
//! message chunks), `tools` (tool calls), `tool_errors` (failed tool calls), `permissions`
//! (requests) and `cards` (each permission card's state in the panel: the chosen option id,
//! `cancelled` or `pending`; comma-separated, `-` for none), `cancelled_ms` (from Stop to the
//! turn's end, `-` without Stop), `builds` (finished during the demo) and `build` (the last
//! one's status, `-` for none), `hunks` (`path:n` per reviewed file relative to the project
//! root, comma-separated, `-` for none), `disk` (`same` when every reviewed document's text
//! equals its file, else the differing paths) and `view` (the active editor's file and the
//! first and last line on screen, `main.tex:20-52`; `-` before its first layout).

use crate::agent::{AgentState, PromptOrigin, bridge, inline, panel, watcher};
use crate::review_ui;
use crate::testmode::wait_until;
use crate::window::WindowState;
use gtk::prelude::{EditableExt as _, TextBufferExt as _, TextViewExt as _, WidgetExt as _};
use lemma_agent::{AgentEvent, PromptBlock, ToolStatus};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// The prompt the demo sends (the scenario's default steps answer it).
pub const DEMO_PROMPT: &str = "Please tidy up the introduction and compile the document.";
/// The environment variable with a prompt to send instead of [`DEMO_PROMPT`].
pub const PROMPT_ENV: &str = "LEMMA_DEMO_PROMPT";
/// The environment variable with the key (`y`, `Y`, `n`) that answers permission cards.
pub const ANSWER_ENV: &str = "LEMMA_DEMO_ANSWER";
/// The environment variable with the delay in milliseconds after which Stop is pressed.
pub const CANCEL_ENV: &str = "LEMMA_DEMO_CANCEL_MS";
/// The environment variable with an instruction to send through the Ctrl+K inline edit.
pub const INLINE_ENV: &str = "LEMMA_DEMO_INLINE";
/// The environment variable with the 1-based line to put the cursor on before Ctrl+K.
pub const LINE_ENV: &str = "LEMMA_DEMO_LINE";
/// The environment variable with the delay in milliseconds after which the demo resolves even
/// though the turn still runs.
pub const RETURN_ENV: &str = "LEMMA_DEMO_RETURN_MS";
/// The start of the report line the demo prints on stdout.
pub const REPORT_PREFIX: &str = "agent-demo:";
/// The environment variable that lets the demo start the real agent (`1`); never set it in tests.
pub const REAL_ENV: &str = "LEMMA_DEMO_REAL_AGENT";
/// The environment variable that stops the demo after the handshake (`1`), before any prompt.
pub const HANDSHAKE_ENV: &str = "LEMMA_DEMO_HANDSHAKE_ONLY";
/// Longest wait for the turn (the scenario may build the document); `LEMMA_DEMO_TURN_TIMEOUT_S`
/// overrides it (a real agent can think for minutes).
fn turn_timeout() -> Duration {
    std::env::var("LEMMA_DEMO_TURN_TIMEOUT_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(120), Duration::from_secs)
}

/// The end-to-end knobs (see the module docs).
#[derive(Debug, Default)]
struct Knobs {
    prompt: Option<String>,
    answer: Option<char>,
    cancel_after: Option<Duration>,
    inline: Option<String>,
    line: Option<u32>,
    return_after: Option<Duration>,
}

impl Knobs {
    fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let millis = |name: &str| -> Result<Option<Duration>, String> {
            var(name)
                .map(|v| {
                    v.parse()
                        .map(Duration::from_millis)
                        .map_err(|_| format!("{name}={v}: expected milliseconds"))
                })
                .transpose()
        };
        Ok(Self {
            prompt: var(PROMPT_ENV),
            answer: match var(ANSWER_ENV).as_deref() {
                None => None,
                Some(key @ ("y" | "Y" | "n")) => key.chars().next(),
                Some(other) => return Err(format!("{ANSWER_ENV}={other}: expected y, Y or n")),
            },
            cancel_after: millis(CANCEL_ENV)?,
            inline: var(INLINE_ENV),
            line: var(LINE_ENV)
                .map(|v| {
                    v.parse::<u32>()
                        .ok()
                        .filter(|&l| l > 0)
                        .ok_or_else(|| format!("{LINE_ENV}={v}: expected a line number"))
                })
                .transpose()?,
            return_after: millis(RETURN_ENV)?,
        })
    }
}

/// What the demo saw of the turn.
#[derive(Default)]
struct Seen {
    stop: Option<String>,
    exited: bool,
    chunks: usize,
    tools: usize,
    tool_errors: usize,
    /// Permission requests not answered yet by the demo.
    pending: VecDeque<u64>,
    permissions: usize,
    /// `title [kind]` per tool call, and the agent's reply text (printed for the real agent).
    tool_titles: Vec<String>,
    message: String,
    /// The `Ready` details: agent, mode, modes and config options.
    ready: Option<String>,
}

/// How [`drive_turn`] stopped waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waited {
    Ended,
    /// At a permission card, which stays for the snapshot.
    AtCard,
    /// [`RETURN_ENV`] was over while the turn still ran.
    Running,
}

/// Runs the agent demo (see the module docs).
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    let agent = state
        .agent_controller()
        .cloned()
        .ok_or("the window has no agent")?;
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    let real = agent.scenario().is_none();
    if real && !flag(REAL_ENV) {
        return Err(
            "--demo agent needs --agent-scenario FILE (it never runs the real agent)".into(),
        );
    }
    let knobs = Knobs::from_env()?;

    let seen: Rc<RefCell<Seen>> = Rc::default();
    let sink = seen.clone();
    let id = agent.connect_event(move |event| {
        let mut seen = sink.borrow_mut();
        match event {
            AgentEvent::Ready {
                agent,
                modes,
                mode,
                options,
                ..
            } => {
                let modes: Vec<&str> = modes.iter().map(|(id, _)| id.as_str()).collect();
                let options: Vec<String> = options
                    .iter()
                    .map(|o| format!("{}={}", o.id, o.current))
                    .collect();
                seen.ready = Some(format!(
                    "agent={agent:?} mode={mode} modes={} options={}",
                    modes.join(","),
                    options.join(",")
                ));
            }
            AgentEvent::TurnEnded(reason) => {
                seen.stop.get_or_insert(format!("{reason:?}"));
            }
            AgentEvent::Permission { request, title, .. } => {
                seen.permissions += 1;
                seen.pending.push_back(*request);
                seen.tool_titles.push(format!("permission: {title}"));
            }
            AgentEvent::Exited(_) => seen.exited = true,
            AgentEvent::MessageChunk(text) => {
                seen.chunks += 1;
                seen.message.push_str(text);
            }
            AgentEvent::ToolCall { title, kind, .. } => {
                seen.tools += 1;
                seen.tool_titles.push(format!("{title} [{kind}]"));
            }
            AgentEvent::ToolUpdate {
                status: Some(ToolStatus::Failed),
                title,
                ..
            } => {
                seen.tool_errors += 1;
                seen.tool_titles
                    .push(format!("failed: {}", title.as_deref().unwrap_or("?")));
            }
            _ => {}
        }
    });

    if knobs.inline.is_none() {
        panel::focus_prompt(&state);
    }
    if let Err(e) = agent.ensure_started().await {
        agent.disconnect(id);
        return Err(e);
    }
    if real && flag(HANDSHAKE_ENV) {
        // Ready arrives with the handshake; give the event loop a moment to deliver it.
        let _ = wait_until(Duration::from_secs(10), "the Ready event", || {
            seen.borrow().ready.is_some()
        })
        .await;
        agent.disconnect(id);
        println!(
            "{REPORT_PREFIX} handshake {}",
            seen.borrow().ready.as_deref().unwrap_or("no Ready event")
        );
        return Ok(());
    }

    let builds = Rc::new(Cell::new(0usize));
    let last_build: Rc<RefCell<Option<String>>> = Rc::default();
    if let Some(build) = state.build.get() {
        let (builds, last_build) = (builds.clone(), last_build.clone());
        build.connect_finished(move |outcome| {
            builds.set(builds.get() + 1);
            *last_build.borrow_mut() = Some(format!("{:?}", outcome.status));
        });
    }

    match &knobs.inline {
        Some(instruction) => {
            if let Err(e) = send_inline(&state, instruction, knobs.line).await {
                agent.disconnect(id);
                return Err(e);
            }
        }
        None => agent.prompt(
            vec![PromptBlock::Text(
                knobs
                    .prompt
                    .clone()
                    .unwrap_or_else(|| DEMO_PROMPT.to_owned()),
            )],
            PromptOrigin::Panel,
        ),
    }
    let sent = Instant::now();

    let result = drive_turn(&state, &seen, &knobs, sent).await;
    agent.disconnect(id);
    let (waited, cancelled_after) = result?;
    if let AgentState::Failed(message) = agent.state() {
        return Err(message);
    }
    if waited == Waited::Ended {
        // Writes by other programs reach the file monitor after its debounce, possibly after
        // the turn's end, and may ask for a build of their own.
        glib::timeout_future(watcher::DEBOUNCE * 2).await;
        wait_until(turn_timeout(), "the file monitor", || {
            watcher::is_idle(&state) && !bridge::late_build_pending(&agent)
        })
        .await?;
    }
    if waited != Waited::Running {
        // The turn-end build, then the review's recompute and the panel's frame.
        if let Some(build) = state.build.get() {
            wait_until(turn_timeout(), "the build", || !build.is_busy()).await?;
        }
        glib::timeout_future(Duration::from_millis(100)).await;
        // Show the first change, as the banner's Review button does.
        if !review_ui::reviewed_documents(&state.workspace).is_empty() {
            review_ui::goto_next(&state, true);
            glib::timeout_future(Duration::from_millis(100)).await;
            // A tab opened for the change scrolls again after its first frame.
            crate::testmode::wait_for_frames(&state.window, 2).await;
        }
    }
    report(
        &state,
        &seen.borrow(),
        waited,
        cancelled_after,
        &builds,
        &last_build,
    );
    if real {
        let seen = seen.borrow();
        for title in &seen.tool_titles {
            println!("agent-demo-tool: {title}");
        }
        println!(
            "agent-demo-message: {}",
            seen.message.replace('\n', " \u{23ce} ")
        );
    }
    Ok(())
}

/// Prints the report line (see the module docs).
fn report(
    state: &Rc<WindowState>,
    seen: &Seen,
    waited: Waited,
    cancelled_after: Option<Duration>,
    builds: &Cell<usize>,
    last_build: &RefCell<Option<String>>,
) {
    let cards: Vec<String> = panel::get(state)
        .map(|panel| {
            panel.flush();
            panel.entries()
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| match entry {
            panel::Entry::Permission(card) => Some(match card.answer {
                panel::PermissionAnswer::Chosen(option) => option,
                panel::PermissionAnswer::Cancelled => "cancelled".to_owned(),
                panel::PermissionAnswer::Pending => "pending".to_owned(),
            }),
            _ => None,
        })
        .collect();
    let summary = review_ui::review_summary(state);
    let root = state.workspace.root().path().to_owned();
    let relative = |path: &std::path::Path| {
        path.strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string()
    };
    let mut hunks = Vec::new();
    let mut differ = Vec::new();
    for doc in &summary.docs {
        let count = doc.review.borrow().as_ref().map_or(0, |r| r.hunks().len());
        hunks.push(format!("{}:{count}", relative(doc.path())));
        if std::fs::read(doc.path()).ok().as_deref() != Some(doc.text().as_bytes()) {
            differ.push(relative(doc.path()));
        }
    }
    let list = |items: &[String]| {
        if items.is_empty() {
            "-".to_owned()
        } else {
            items.join(",")
        }
    };
    // What the active editor shows: its file and the first and last line on screen.
    let view = state.active_editor().map_or_else(
        || "-".to_owned(),
        |editor| {
            let view = editor.view();
            let rect = view.visible_rect();
            if rect.height() <= 0 {
                // Not laid out yet.
                return "-".to_owned();
            }
            let first = view.line_at_y(rect.y()).0.line() + 1;
            let last = view.line_at_y(rect.y() + rect.height() - 1).0.line() + 1;
            format!("{}:{first}-{last}", relative(editor.document().path()))
        },
    );
    let stop = match waited {
        Waited::Ended => seen.stop.clone().unwrap_or_default(),
        Waited::AtCard => "permission".to_owned(),
        Waited::Running => "running".to_owned(),
    };
    println!(
        "{REPORT_PREFIX} stop={stop} turn={} chunks={} tools={} tool_errors={} permissions={} \
         cards={} cancelled_ms={} builds={} build={} hunks={} disk={} view={view}",
        state.agent_controller().map_or(0, |a| a.turn_id()),
        seen.chunks,
        seen.tools,
        seen.tool_errors,
        seen.permissions,
        list(&cards),
        cancelled_after.map_or("-".to_owned(), |d| d.as_millis().to_string()),
        builds.get(),
        last_build.borrow().as_deref().unwrap_or("-"),
        list(&hunks),
        if differ.is_empty() {
            "same".to_owned()
        } else {
            differ.join(",")
        },
    );
}

/// Sends `instruction` as the user does with Ctrl+K: the cursor goes to `line` (1-based) of the
/// active editor, `win.agent-inline-edit` opens the popover on the paragraph there, the
/// instruction is typed into its entry, and Enter sends it.
async fn send_inline(
    state: &Rc<WindowState>,
    instruction: &str,
    line: Option<u32>,
) -> Result<(), String> {
    let editor = state
        .active_editor()
        .ok_or("the inline edit needs an open document")?;
    let buffer = editor.buffer();
    if let Some(line) = line {
        // A view scrolled before its first layout can end up elsewhere.
        crate::testmode::wait_for_frames(&state.window, 1).await;
        let line = i32::try_from(line - 1).map_err(|_| format!("line {line} is too large"))?;
        let iter = buffer
            .iter_at_line(line)
            .ok_or_else(|| format!("the document has no line {}", line + 1))?;
        buffer.place_cursor(&iter);
        editor
            .view()
            .scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.3);
        // The scroll happens during the next layout; the popover points at where the region is.
        crate::testmode::wait_for_frames(&state.window, 2).await;
    }
    editor.view().grab_focus();
    activate(state, "agent-inline-edit");
    let entry = inline::entry(state).ok_or("Ctrl+K did not open the inline edit")?;
    wait_until(Duration::from_secs(10), "the inline-edit popover", || {
        inline::popover(state).is_some_and(|p| p.is_mapped())
    })
    .await?;
    entry.set_text(instruction);
    inline::submit(state, instruction);
    Ok(())
}

/// Activates the window action `win.<name>`, as its key or button does.
fn activate(state: &WindowState, name: &str) {
    gio::prelude::ActionGroupExt::activate_action(&state.window, name, None);
}

/// Waits for the turn to end (or for the first permission card when there is no answer key, or
/// until [`RETURN_ENV`] is over), answering cards and pressing Stop on the way. Returns how it
/// stopped waiting and the time from Stop to the turn's end.
async fn drive_turn(
    state: &Rc<WindowState>,
    seen: &Rc<RefCell<Seen>>,
    knobs: &Knobs,
    sent: Instant,
) -> Result<(Waited, Option<Duration>), String> {
    let deadline = sent + turn_timeout();
    let mut stopped_at: Option<Instant> = None;
    let waited = loop {
        {
            let seen = seen.borrow();
            if seen.stop.is_some() {
                break Waited::Ended;
            }
            if seen.exited {
                return Err("the agent exited".into());
            }
            if knobs.answer.is_none() && knobs.cancel_after.is_none() && !seen.pending.is_empty() {
                break Waited::AtCard;
            }
        }
        if let Some(AgentState::Failed(message)) = state.agent_controller().map(|a| a.state()) {
            return Err(message);
        }
        if knobs
            .return_after
            .is_some_and(|after| sent.elapsed() >= after)
        {
            break Waited::Running;
        }
        if Instant::now() > deadline {
            return Err("timed out waiting for the agent's turn".into());
        }
        if let Some(after) = knobs.cancel_after
            && stopped_at.is_none()
            && sent.elapsed() >= after
        {
            stopped_at = Some(Instant::now());
            activate(state, "agent-stop");
        }
        if let Some(key) = knobs.answer {
            let request = seen.borrow_mut().pending.pop_front();
            if let Some(request) = request {
                answer_card(state, request, key).await?;
            }
        }
        glib::timeout_future(Duration::from_millis(10)).await;
    };
    let cancelled_after = match waited {
        Waited::Ended => stopped_at.map(|at| at.elapsed()),
        _ => None,
    };
    Ok((waited, cancelled_after))
}

/// Answers permission request `request` with `key` the way the user does: `win.agent-focus`
/// moves the focus to the pending card, then the panel's key handler gets the key, and the card
/// must record a chosen option.
async fn answer_card(state: &Rc<WindowState>, request: u64, key: char) -> Result<(), String> {
    let panel = panel::get(state).ok_or("the window has no agent panel")?;
    // The card is added on the panel's next frame.
    wait_until(Duration::from_secs(10), "the permission card", || {
        panel.flush();
        !panel.permission_buttons(request).is_empty()
    })
    .await?;
    activate(state, "agent-focus");
    let keyval =
        gtk::gdk::Key::from_name(key.to_string()).ok_or_else(|| format!("no key for {key:?}"))?;
    // `Y` is Shift+y, as the keyboard sends it (the panel needs Shift for "always").
    let modifiers = if key.is_ascii_uppercase() {
        gtk::gdk::ModifierType::SHIFT_MASK
    } else {
        gtk::gdk::ModifierType::empty()
    };
    if panel.key_pressed(keyval, modifiers) != glib::Propagation::Stop {
        return Err(format!(
            "the panel did not take the key {key:?} for permission request {request}"
        ));
    }
    let chosen = panel.entries().into_iter().find_map(|entry| match entry {
        panel::Entry::Permission(card) if card.request == request => Some(card.answer),
        _ => None,
    });
    match chosen {
        Some(panel::PermissionAnswer::Chosen(_)) => Ok(()),
        other => Err(format!(
            "permission request {request} was not answered by {key:?}: {other:?}"
        )),
    }
}
