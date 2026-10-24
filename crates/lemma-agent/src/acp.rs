//! The ACP client (WP2.1; ARCHITECTURE §3, §10.3 `spawn`, §10.4). Every use of
//! `agent-client-protocol` in the workspace is in this file.
//!
//! # Threads
//!
//! [`spawn`] starts the agent process (`program args`: `node <adapter.js>`, an agent's own command
//! line tool such as `grok agent stdio`, or `lemma-fake-agent <scenario>` in tests) as the leader
//! of its own process group, then these threads, all named `lemma-agent*`:
//!
//! | Thread | Job |
//! |---|---|
//! | `lemma-agent` | the ACP client: `futures_executor::block_on` over the SDK connection (no tokio, no reactor) |
//! | `lemma-agent-out` | reads the agent's stdout lines into the connection |
//! | `lemma-agent-in` | writes the connection's lines to the agent's stdin |
//! | `lemma-agent-err` | keeps the last 64 KiB of the agent's stderr |
//! | `lemma-agent-wait` | waits for the agent to exit and reaps it |
//!
//! When `spawn` started the MCP server ([`crate::mcp::McpServer`]), its threads run as well and
//! the server is stopped when the session ends. The SDK runs on our own line transport, so no
//! runtime threads outlive a session: when the events channel closes, every thread above has
//! finished (or, if a stray process still holds a pipe open after the group was killed, has been
//! detached; this is logged).
//!
//! # Events
//!
//! In order, on one thread: `Ready` first (updates that arrive during start-up are delivered after
//! it), then the session's events, and `Exited(code)` last; the channel then closes. Each accepted
//! `Prompt` gets exactly one `TurnEnded`: the agent's stop reason; `Cancelled` after `Cancel` or
//! `Shutdown`; `EndTurn` when the turn ends without a stop reason (the agent answered the prompt
//! with an error, reported first as `Error`, or the agent died). A `Prompt` while a turn runs is
//! refused with an `Error` and gets no `TurnEnded`.
//!
//! # Lemma's policy on modes and permissions (risk §9.6)
//!
//! With Claude Code only the `default` and `plan` modes are offered; with other agents
//! ([`AcpFlavor::Generic`]) every mode they have except [`ELEVATED_MODES`] (see
//! [`AcpFlavor::offers_mode`]). `Ready.modes`, the `mode` config option and `SetMode`/`SetConfig`
//! are limited to the offered modes; a session that starts in another mode, or that the agent
//! switches to another mode, is switched back to the session's fallback mode (`default`, or for an
//! agent without one the first mode it offers; with a `Notice` in the second case; a switch
//! during start-up is reverted right after `Ready`, which reports it). Permission
//! options that leave plan mode for an elevated mode (`exit-plan-auto`, `exit-plan-bypass`,
//! `exit-plan-accept-edits` and their `exit-plan-clear-*` variants) are not offered;
//! `exit-plan-default` and `reject` remain. A permission request the agent withdraws
//! (`$/cancel_request`) is answered with the request-cancelled error and reported as
//! `PermissionWithdrawn`.
//!
//! # `fs/*` routing ([`AcpFlavor::Generic`], WP5.2)
//!
//! For adapters other than Claude's, `initialize` advertises `fs.readTextFile` and
//! `fs.writeTextFile`. `fs/read_text_file` becomes [`EditorRequest::ReadText`] and
//! `fs/write_text_file` [`EditorRequest::WriteText`], each a [`ToolCall`] with a fresh call id on
//! `handle.tools` (which then stays open even when the caller serves the MCP socket itself); the
//! main thread resolves the path through `ProjectRoot::resolve` and answers like an MCP tool. A
//! reply that is an error, or none within [`mcp::TOOL_TIMEOUT`], is a JSON-RPC error with the
//! reply's text. Requests for another session are refused. With [`AcpFlavor::Claude`] nothing is
//! advertised and such requests are refused. Our MCP server is passed in `session/new` either way.
//!
//! An agent error with ACP's "authentication required" code (-32000) gets
//! [`crate::LOGIN_REQUIRED`] appended to its message.
//!
//! # Instructions
//!
//! `system_prompt_append` goes in `session/new`'s `_meta.systemPrompt.append` (Claude's adapter
//! reads it; others ignore it). ACP itself has no system prompt, so for [`AcpFlavor::Generic`]
//! it is also sent as the first text block of the session's first prompt, between
//! [`INSTRUCTIONS_TAG`] and `</lemma_instructions>`.
//!
//! # Process lifecycle
//!
//! - **Death.** When the agent exits, what it wrote last is still delivered (until its stdout
//!   reaches EOF, at most [`DRAIN_WAIT`] after the exit). Then (or as soon as it closes its
//!   stdout) a running turn ends, start-up failures are reported as `Error` with the stderr tail,
//!   the rest of the process group gets SIGTERM and then SIGKILL, the leader is reaped, and
//!   `Exited(code)` follows (`None` when a signal ended it). Later commands fail: the command
//!   channel is closed.
//! - **Shutdown** (also when every command sender is dropped): pending permissions are answered
//!   `Cancelled`, a running turn gets `session/cancel`, the session gets `session/close` (when the
//!   agent supports it), stdin is closed; after [`EXIT_GRACE`] the group gets SIGTERM, after
//!   [`TERM_GRACE`] more SIGKILL; then the threads are joined. This takes well under a second.

use crate::{
    AcpFlavor, AgentCmd, AgentConfig, AgentEvent, AgentHandle, ConfigOpt, EditorRequest,
    PermOption, PromptBlock, StopReason, ToolCall, ToolReply, ToolStatus, mcp,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1 as s;
use agent_client_protocol::{Agent, Client, ConnectionTo, Lines, Responder};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::future::{Future, poll_fn};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::panic::AssertUnwindSafe;
use std::pin::{Pin, pin};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Size of the stderr ring buffer.
pub const STDERR_RING_BYTES: usize = 64 * 1024;
/// After stdin is closed at `Shutdown`, how long the agent may take to exit by itself.
pub const EXIT_GRACE: Duration = Duration::from_millis(300);
/// After SIGTERM, how long the process group may take to exit before SIGKILL.
pub const TERM_GRACE: Duration = Duration::from_millis(300);
/// The modes Lemma offers with Claude Code (§10.4).
pub const OFFERED_MODES: &[&str] = &["default", "plan"];
/// How the block of Lemma's instructions starts in the first prompt of an agent other than
/// Claude Code (see the module docs).
pub const INSTRUCTIONS_TAG: &str = "<lemma_instructions>";
/// The modes Lemma never offers with other agents ([`AcpFlavor::Generic`]): they let the agent
/// run commands without asking, or leave its sandbox (Codex's `agent-full-access`, Gemini CLI's
/// `yolo`), plus the modes of Claude Code's that Lemma does not offer, should another agent
/// use those names. (Edits never need the agent's own approval: the user reviews them in Lemma.)
pub const ELEVATED_MODES: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "agent-full-access",
    "full-access",
    "yolo",
];
/// Permission options that would switch the session to a mode Lemma does not offer.
const ELEVATING_OPTIONS: &[&str] = &[
    "exit-plan-auto",
    "exit-plan-bypass",
    "exit-plan-accept-edits",
    "exit-plan-clear-auto",
    "exit-plan-clear-bypass",
    "exit-plan-clear-accept-edits",
];
/// After the agent exited, how long its last output may take to arrive (stdout EOF) before the
/// session is ended anyway (a stray process may still hold the pipe).
const DRAIN_WAIT: Duration = Duration::from_millis(300);
/// After SIGKILL, how long to wait for the leader before giving up on reaping it.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// How long finished I/O threads may take to be joined at the end.
const JOIN_WAIT: Duration = Duration::from_millis(500);

const CLIENT_NAME: &str = "lemma";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// See [`crate::spawn`].
pub(crate) fn spawn(cfg: AgentConfig) -> io::Result<AgentHandle> {
    let socket = cfg.mcp_socket.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the MCP socket path is not UTF-8",
        )
    })?;
    let socket = socket.to_owned();
    let raw_log = match &cfg.raw_log {
        Some(path) => Some(RawLog::create(path)?),
        None => None,
    };

    let (cmd_tx, cmd_rx) = async_channel::unbounded::<AgentCmd>();
    let (events_tx, events_rx) = async_channel::unbounded::<AgentEvent>();
    let (tools_tx, tools_rx) = async_channel::unbounded::<ToolCall>();

    // `fs/*` requests of a generic adapter go to `handle.tools` (see the module docs).
    let fs_tools = (cfg.flavor == AcpFlavor::Generic).then(|| tools_tx.clone());
    // The MCP server: ours, unless the caller already serves this socket.
    let mcp_server = if mcp_server_listening(&cfg.mcp_socket) {
        tracing::debug!(%socket, "an MCP server already listens; tool calls go to its channel");
        drop(tools_tx);
        None
    } else {
        // A stale socket file (nobody listening) is replaced by the server.
        Some(mcp::McpServer::start(
            &cfg.mcp_socket,
            cfg.mcp_token.clone(),
            tools_tx,
        )?)
    };

    let mut command = Command::new(&cfg.program);
    command.args(&cfg.args);
    if cfg.flavor == AcpFlavor::Claude {
        // Claude Code reuses the user's login; an API key meant for the API-key backend would
        // make it bill that key instead. (Pi's keys belong to Pi: generic adapters keep it.)
        command.env_remove(crate::keys::ENV_VAR);
    }
    command.envs(cfg.env.iter().map(|(k, v)| (k, v)));
    command
        // A Claude Code session in the terminal that started Lemma must not leak into ours.
        .env_remove("CLAUDECODE")
        .current_dir(&cfg.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    let spawned = command.process_group(0).spawn();
    // On Windows the agent and everything it starts run in a job object instead.
    #[cfg(windows)]
    let spawned = lemma_core::process::Job::spawn(&mut command);
    let leader = match spawned {
        Ok(leader) => leader,
        Err(e) => {
            if let Some(server) = &mcp_server {
                server.stop();
            }
            return Err(io::Error::new(
                e.kind(),
                format!("cannot start {}: {e}", cfg.program.display()),
            ));
        }
    };
    #[cfg(unix)]
    let mut child = leader;
    #[cfg(windows)]
    let (mut child, job) = leader;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        unreachable!("all three pipes were requested");
    };
    #[cfg(unix)]
    let leader = {
        let pid = child.id() as libc::pid_t;
        // `child` is never waited on through std: the wait thread reaps the pid itself.
        drop(child);
        pid
    };
    #[cfg(windows)]
    let leader = Leader { child, job };

    // From here on, a failure (no thread could be started) must not leave the process behind:
    // dropping `process` kills its group and lets the wait thread reap it.
    let (exit_tx, exit_rx) = async_channel::bounded::<ProcSignal>(2);
    #[cfg(unix)]
    let pid = leader;
    let process = match AgentProcess::watch(leader, exit_tx) {
        Ok(process) => process,
        Err(e) => {
            // SAFETY: plain syscalls on our own, not yet reaped child.
            #[cfg(unix)]
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
            #[cfg(unix)]
            reap(pid);
            // On Windows the job, dropped with the leader, has ended the processes.
            return Err(e);
        }
    };
    let stderr_ring = Arc::new(Mutex::new(StderrRing::default()));
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let (incoming_tx, incoming_rx) = async_channel::unbounded::<io::Result<String>>();
    let io_threads = vec![
        named("lemma-agent-out", {
            let log = raw_log.clone();
            move || read_stdout(stdout, &incoming_tx, log.as_ref())
        })?,
        named("lemma-agent-in", {
            let log = raw_log.clone();
            move || write_stdin(stdin, &line_rx, log.as_ref())
        })?,
        named("lemma-agent-err", {
            let ring = stderr_ring.clone();
            let log = raw_log.clone();
            move || read_stderr(stderr, &ring, log.as_ref())
        })?,
    ];

    let shared = Arc::new(Shared::new(events_tx, process.exited_flag(), cfg.flavor));
    if cfg.flavor == AcpFlavor::Generic && !cfg.system_prompt_append.is_empty() {
        shared.lock().instructions = Some(cfg.system_prompt_append.clone());
    }
    let fs = Arc::new(FsRouter {
        tools: fs_tools,
        shared: shared.clone(),
    });
    let session = Session {
        cfg,
        socket,
        shared,
        fs,
        cmd_rx,
        exit_rx,
    };
    let agent = AgentThread {
        session,
        process,
        io_threads,
        line_tx,
        incoming_rx,
        stderr_ring,
        mcp_server,
    };
    thread::Builder::new()
        .name("lemma-agent".into())
        .spawn(move || agent.run())?;
    Ok(AgentHandle {
        cmd: cmd_tx,
        events: events_rx,
        tools: tools_rx,
    })
}

/// True when something accepts connections on `socket` (the caller started its own server).
fn mcp_server_listening(socket: &std::path::Path) -> bool {
    crate::transport::connect(socket).is_ok()
}

fn named(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.into()).spawn(f)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// =================================================================================== the agent

/// Everything the `lemma-agent` thread owns.
struct AgentThread {
    session: Session,
    process: AgentProcess,
    io_threads: Vec<JoinHandle<()>>,
    line_tx: mpsc::Sender<String>,
    incoming_rx: async_channel::Receiver<io::Result<String>>,
    stderr_ring: Arc<Mutex<StderrRing>>,
    mcp_server: Option<mcp::McpServer>,
}

/// How the connection ended.
enum End {
    /// `Shutdown`, or every command sender was dropped.
    Shutdown,
    /// The agent exited or closed its stdout.
    Died,
    /// Start-up failed with this message (the agent is still running).
    StartFailed(String),
    /// The SDK connection failed.
    Failed(String),
    /// The agent thread panicked.
    Panicked(String),
}

impl AgentThread {
    fn run(self) {
        let AgentThread {
            session,
            mut process,
            io_threads,
            line_tx,
            incoming_rx,
            stderr_ring,
            mcp_server,
        } = self;
        let shared = session.shared.clone();
        let sink = LineSink { tx: Some(line_tx) };
        let end = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            futures_executor::block_on(session.connect(sink, incoming_rx))
        })) {
            Ok(Ok(end)) => end,
            Ok(Err(e)) => End::Failed(e.to_string()),
            Err(panic) => End::Panicked(panic_message(&panic)),
        };
        let died_by_itself = process.has_exited();
        let grace = if matches!(end, End::Shutdown | End::StartFailed(_)) {
            EXIT_GRACE
        } else {
            Duration::ZERO
        };
        let code = process.terminate(grace);
        if let Some(server) = &mcp_server {
            server.stop();
        }
        drop(mcp_server);
        join_all(io_threads, JOIN_WAIT);

        let stderr = lock(&stderr_ring).text();
        let mut st = shared.lock();
        let was_ready = st.ready;
        // From here on events go out directly; anything buffered before `Ready` is dropped.
        st.ready = true;
        st.pre_ready.clear();
        let mut final_events = Vec::new();
        if let Some(turn) = st.turn.take() {
            let reason = if turn.cancel_requested || matches!(end, End::Shutdown) {
                StopReason::Cancelled
            } else {
                StopReason::EndTurn
            };
            final_events.push(AgentEvent::TurnEnded(reason));
        }
        // Permission responders die with the connection; the UI's cards are moot after Exited.
        st.permissions.clear();
        let end = match end {
            // A connection error after the agent died is just its death.
            End::Failed(_) if died_by_itself => End::Died,
            other => other,
        };
        match end {
            End::Shutdown => {}
            End::Died if was_ready => {
                if !stderr.is_empty() {
                    tracing::warn!("the agent exited ({code:?}); its stderr ended with:\n{stderr}");
                }
            }
            End::Died => final_events.push(AgentEvent::Error(startup_death_message(code, &stderr))),
            End::StartFailed(msg) => {
                if !stderr.is_empty() {
                    tracing::warn!(
                        "agent start-up failed: {msg}; its stderr ended with:\n{stderr}"
                    );
                }
                final_events.push(AgentEvent::Error(msg));
            }
            End::Failed(msg) => final_events.push(AgentEvent::Error(format!(
                "The connection to the agent failed: {msg}"
            ))),
            End::Panicked(msg) => final_events.push(AgentEvent::Error(format!(
                "The agent connection stopped after an internal error: {msg}"
            ))),
        }
        final_events.push(AgentEvent::Exited(code));
        for event in final_events {
            let _ = shared.events.try_send(event);
        }
        drop(st);
        // Dropping the last sender closes the events channel: the session is over.
        drop(shared);
    }
}

fn startup_death_message(code: Option<i32>, stderr: &str) -> String {
    let how = match code {
        Some(c) => format!("exit code {c}"),
        None => "killed by a signal".to_owned(),
    };
    if stderr.is_empty() {
        format!("The agent exited during start-up ({how}) without error output.")
    } else {
        format!("The agent exited during start-up ({how}):\n{stderr}")
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_owned()
    }
}

/// Joins the threads that finish within `wait`; detaches (and logs) the rest.
fn join_all(threads: Vec<JoinHandle<()>>, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut pending = threads;
    loop {
        let (done, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(JoinHandle::is_finished);
        for handle in done {
            let _ = handle.join();
        }
        pending = rest;
        if pending.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            for handle in pending {
                tracing::warn!(
                    thread = handle.thread().name().unwrap_or("?"),
                    "an agent I/O thread is still blocked; detaching it"
                );
            }
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

// ============================================================================ the ACP session

/// The connection-side state of one session.
struct Session {
    cfg: AgentConfig,
    socket: String,
    shared: Arc<Shared>,
    fs: Arc<FsRouter>,
    cmd_rx: async_channel::Receiver<AgentCmd>,
    exit_rx: async_channel::Receiver<ProcSignal>,
}

/// Why start-up stopped.
enum StartError {
    /// The agent went away (its stdout closed while a request was pending).
    Died,
    /// The agent refused or failed; this is the message for the user.
    Failed(String),
}

impl StartError {
    fn from_acp(context: &str, e: &agent_client_protocol::Error) -> Self {
        if agent_client_protocol::is_incoming_transport_closed(e) {
            Self::Died
        } else {
            Self::Failed(format!("{context}: {}", describe(e)))
        }
    }
}

/// What start-up learned.
struct Started {
    session_id: s::SessionId,
    can_close: bool,
    /// The agent accepts `image` blocks in prompts (`promptCapabilities.image`).
    images: bool,
}

impl Session {
    async fn connect(
        self,
        sink: LineSink,
        incoming: async_channel::Receiver<io::Result<String>>,
    ) -> Result<End, agent_client_protocol::Error> {
        let on_update = self.shared.clone();
        let on_permission = self.shared.clone();
        let on_read = self.fs.clone();
        let on_write = self.fs.clone();
        Client
            .builder()
            .name(CLIENT_NAME)
            .on_receive_notification(
                async move |n: s::SessionNotification, cx: ConnectionTo<Agent>| {
                    on_update.on_update(n, &cx);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |req: s::RequestPermissionRequest,
                            responder: Responder<s::RequestPermissionResponse>,
                            cx: ConnectionTo<Agent>| {
                    on_permission.on_permission(req, responder, &cx);
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: s::ReadTextFileRequest,
                            responder: Responder<s::ReadTextFileResponse>,
                            _cx: ConnectionTo<Agent>| {
                    on_read.read(req, responder);
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: s::WriteTextFileRequest,
                            responder: Responder<s::WriteTextFileResponse>,
                            _cx: ConnectionTo<Agent>| {
                    on_write.write(req, responder);
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                Lines::new(sink, CooperativeLines::new(incoming)),
                async move |cx: ConnectionTo<Agent>| Ok(self.main(cx).await),
            )
            .await
    }

    /// Start-up, then the command loop.
    async fn main(self, cx: ConnectionTo<Agent>) -> End {
        let shared = self.shared.clone();
        let mut queued: VecDeque<AgentCmd> = VecDeque::new();
        let started = {
            let mut startup = pin!(self.start(&cx));
            loop {
                let cmd = self.cmd_rx.recv();
                let exit = self.exit_rx.recv();
                let closed = cx.incoming_closed();
                match race4(startup.as_mut(), cmd, exit, closed).await {
                    Race::A(Ok(started)) => break started,
                    Race::A(Err(StartError::Died)) => return End::Died,
                    Race::A(Err(StartError::Failed(msg))) => return End::StartFailed(msg),
                    Race::B(Ok(AgentCmd::Shutdown) | Err(_)) => return End::Shutdown,
                    Race::B(Ok(cmd)) => queued.push_back(cmd),
                    Race::C(_) => {
                        self.drain_after_exit(&cx).await;
                        return End::Died;
                    }
                    Race::D(()) => return End::Died,
                }
            }
        };
        shared.become_ready(&started, &cx);
        while let Some(cmd) = queued.pop_front() {
            if self.command(&cx, &started, cmd).is_break() {
                self.close(&cx, &started);
                return End::Shutdown;
            }
        }
        loop {
            let cmd = self.cmd_rx.recv();
            let exit = self.exit_rx.recv();
            let closed = cx.incoming_closed();
            match race3(cmd, exit, closed).await {
                Race::A(Ok(cmd)) => {
                    if self.command(&cx, &started, cmd).is_break() {
                        self.close(&cx, &started);
                        return End::Shutdown;
                    }
                }
                Race::A(Err(_)) => {
                    // Every handle was dropped: nobody is listening any more.
                    self.close(&cx, &started);
                    return End::Shutdown;
                }
                Race::B(_) => {
                    self.drain_after_exit(&cx).await;
                    return End::Died;
                }
                Race::C(()) | Race::D(()) => return End::Died,
            }
        }
    }

    /// The agent exited: let the output it wrote last through the connection (until its stdout
    /// reaches EOF), but no longer than [`DRAIN_WAIT`] after the exit.
    async fn drain_after_exit(&self, cx: &ConnectionTo<Agent>) {
        let deadline = async {
            // The wait thread sends `Deadline` (or closes the channel) DRAIN_WAIT after the exit.
            while let Ok(ProcSignal::Exited) = self.exit_rx.recv().await {}
        };
        race3(cx.incoming_closed(), deadline, std::future::pending::<()>()).await;
    }

    async fn start(&self, cx: &ConnectionTo<Agent>) -> Result<Started, StartError> {
        let init = cx
            .send_request(
                s::InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(
                        // §10.4: fs routing for generic adapters only (WP5.2), no terminal; we
                        // show notices.
                        s::ClientCapabilities::new()
                            .fs(s::FileSystemCapabilities::new()
                                .read_text_file(self.fs.tools.is_some())
                                .write_text_file(self.fs.tools.is_some()))
                            .terminal(false)
                            .session(
                                s::ClientSessionCapabilities::new()
                                    .notices(s::NoticeCapabilities::new()),
                            ),
                    )
                    .client_info(
                        s::Implementation::new(CLIENT_NAME, CLIENT_VERSION).title("Lemma"),
                    ),
            )
            .block_task()
            .await
            .map_err(|e| StartError::from_acp("The agent did not initialize", &e))?;
        if init.protocol_version != ProtocolVersion::V1 {
            return Err(StartError::Failed(format!(
                "The agent speaks ACP version {:?}; Lemma needs version 1.",
                init.protocol_version
            )));
        }
        let agent = init.agent_info.as_ref().map_or_else(
            || "unknown agent".to_owned(),
            |info| format!("{} {}", info.name, info.version),
        );
        let can_close = init.agent_capabilities.session_capabilities.close.is_some();
        let images = init.agent_capabilities.prompt_capabilities.image;

        let bridge = s::McpServerStdio::new("lemma", self.cfg.mcp_bridge.clone()).env(vec![
            s::EnvVariable::new("LEMMA_MCP_SOCKET", self.socket.clone()),
            s::EnvVariable::new("LEMMA_MCP_TOKEN", self.cfg.mcp_token.clone()),
        ]);
        // The response is recorded in an ordered callback, before the agent's next message is
        // dispatched: updates the agent sends right after it (even in the same write) then apply
        // on top of it instead of being overwritten when this task resumes.
        let (new_tx, new_rx) = async_channel::bounded(1);
        let shared = self.shared.clone();
        cx.prepare_request(
            s::NewSessionRequest::new(self.cfg.cwd.clone())
                .mcp_servers(vec![s::McpServer::Stdio(bridge)])
                .meta(session_meta(&self.cfg.system_prompt_append)),
        )
        .on_receiving_result(
            move |result: Result<s::NewSessionResponse, agent_client_protocol::Error>| async move {
                if let Ok(new) = &result {
                    shared.begin_session(&agent, new);
                }
                let _ = new_tx.try_send(result);
                Ok(())
            },
        )
        .map_err(|e| StartError::from_acp("The agent could not start a session", &e))?;
        let new = new_rx
            .recv()
            .await
            .map_err(|_| StartError::Died)?
            .map_err(|e| StartError::from_acp("The agent could not start a session", &e))?;
        let session_id = new.session_id.clone();
        let mode = new
            .modes
            .as_ref()
            .map(|m| m.current_mode_id.0.to_string())
            .unwrap_or_default();
        if new.modes.is_some() && !self.cfg.flavor.offers_mode(&mode) {
            let Some(fallback) = self.shared.lock().fallback_mode.clone() else {
                return Err(StartError::Failed(match self.cfg.flavor {
                    AcpFlavor::Claude => format!(
                        "The agent started in mode \"{mode}\" and offers no \"default\" mode."
                    ),
                    AcpFlavor::Generic => format!(
                        "The agent started in mode \"{mode}\" and offers no mode that Lemma \
                         offers."
                    ),
                }));
            };
            let before = self.shared.mode_generation();
            cx.send_request(s::SetSessionModeRequest::new(
                session_id.clone(),
                fallback.clone(),
            ))
            .block_task()
            .await
            .map_err(|e| {
                StartError::from_acp(
                    &format!("The agent started in mode \"{mode}\", which Lemma does not offer, and could not switch to \"{fallback}\""),
                    &e,
                )
            })?;
            self.shared.set_mode_silently(&fallback, before);
        }
        Ok(Started {
            session_id,
            can_close,
            images,
        })
    }

    /// Handles one command; `Break` means shut down.
    fn command(
        &self,
        cx: &ConnectionTo<Agent>,
        started: &Started,
        cmd: AgentCmd,
    ) -> std::ops::ControlFlow<()> {
        let shared = &self.shared;
        let sid = &started.session_id;
        match cmd {
            AgentCmd::Prompt(blocks) => {
                let Some(seq) = shared.begin_turn() else {
                    shared.emit(AgentEvent::Error(
                        "The agent is still working on the previous prompt.".into(),
                    ));
                    return std::ops::ControlFlow::Continue(());
                };
                if !started.images
                    && let Some(notice) = no_images_notice(&blocks)
                {
                    shared.emit(AgentEvent::Notice(notice.into()));
                }
                // Lemma's instructions open the session's first prompt (see the module docs).
                let instructions = shared.lock().instructions.take();
                let sent = cx.send_request(s::PromptRequest::new(
                    sid.clone(),
                    instructions
                        .map(|text| {
                            s::ContentBlock::Text(s::TextContent::new(format!(
                                "{INSTRUCTIONS_TAG}\n{text}\n</lemma_instructions>"
                            )))
                        })
                        .into_iter()
                        .chain(blocks.into_iter().map(|b| content_block(b, started.images)))
                        .collect(),
                ));
                let shared = shared.clone();
                self.spawn(cx, async move {
                    let result = sent.block_task().await;
                    shared.finish_turn(seq, result);
                });
            }
            AgentCmd::Cancel => {
                shared.answer_all_cancelled();
                if shared.request_cancel() {
                    let _ = cx.send_notification(s::CancelNotification::new(sid.clone()));
                }
            }
            AgentCmd::Permission { request, option_id } => {
                shared.answer_permission(request, option_id);
            }
            AgentCmd::SetMode(mode) => {
                if !self.cfg.flavor.offers_mode(&mode) {
                    shared.emit(AgentEvent::Error(format!(
                        "Lemma does not offer the mode \"{mode}\"."
                    )));
                    return std::ops::ControlFlow::Continue(());
                }
                let before = shared.mode_generation();
                let sent =
                    cx.send_request(s::SetSessionModeRequest::new(sid.clone(), mode.clone()));
                let shared = shared.clone();
                self.spawn(cx, async move {
                    match sent.block_task().await {
                        Ok(_) => shared.mode_confirmed(&mode, before),
                        Err(e) => shared
                            .request_failed(&e, &format!("Could not switch to mode \"{mode}\"")),
                    }
                });
            }
            AgentCmd::SetConfig { id, value } => {
                if id == "mode" && !self.cfg.flavor.offers_mode(&value) {
                    shared.emit(AgentEvent::Error(format!(
                        "Lemma does not offer the mode \"{value}\"."
                    )));
                    return std::ops::ControlFlow::Continue(());
                }
                let config_value = if shared.is_boolean_option(&id) {
                    s::SessionConfigOptionValue::boolean(value == "true")
                } else {
                    s::SessionConfigOptionValue::value_id(value.clone())
                };
                let sent = cx.send_request(s::SetSessionConfigOptionRequest::new(
                    sid.clone(),
                    id.clone(),
                    config_value,
                ));
                let shared = shared.clone();
                self.spawn(cx, async move {
                    match sent.block_task().await {
                        Ok(response) => shared.config_confirmed(&response.config_options),
                        Err(e) => {
                            shared
                                .request_failed(&e, &format!("Could not set {id} to \"{value}\""));
                        }
                    }
                });
            }
            AgentCmd::Shutdown => return std::ops::ControlFlow::Break(()),
        }
        std::ops::ControlFlow::Continue(())
    }

    /// Runs `task` on the connection; it never fails the connection.
    fn spawn(&self, cx: &ConnectionTo<Agent>, task: impl Future<Output = ()> + Send + 'static) {
        if let Err(e) = cx.spawn(async move {
            task.await;
            Ok(())
        }) {
            tracing::warn!("cannot start an agent request task: {e}");
        }
    }

    /// The polite part of `Shutdown`: answer permissions, cancel the turn, close the session.
    fn close(&self, cx: &ConnectionTo<Agent>, started: &Started) {
        let shared = &self.shared;
        shared.answer_all_cancelled();
        if shared.request_cancel() {
            let _ = cx.send_notification(s::CancelNotification::new(started.session_id.clone()));
        }
        if started.can_close {
            cx.send_request(s::CloseSessionRequest::new(started.session_id.clone()))
                .detach();
        }
    }
}

/// `e` for the user; an "authentication required" error (-32000) gets [`crate::LOGIN_REQUIRED`].
fn describe(e: &agent_client_protocol::Error) -> String {
    if e.code == agent_client_protocol::ErrorCode::AuthRequired {
        format!("{e} {}", crate::LOGIN_REQUIRED)
    } else {
        e.to_string()
    }
}

/// Routes the agent's `fs/*` requests to the main thread (see the module docs).
struct FsRouter {
    /// `handle.tools`, for [`AcpFlavor::Generic`]; `None` refuses every `fs/*` request.
    tools: Option<async_channel::Sender<ToolCall>>,
    shared: Arc<Shared>,
}

impl FsRouter {
    fn read(&self, req: s::ReadTextFileRequest, responder: Responder<s::ReadTextFileResponse>) {
        let request = EditorRequest::ReadText {
            path: req.path,
            line: req.line,
            limit: req.limit,
        };
        self.route(&req.session_id, request, move |answer| {
            let _ = match answer {
                Ok(text) => responder.respond(s::ReadTextFileResponse::new(text)),
                Err(e) => responder.respond_with_error(e),
            };
        });
    }

    fn write(&self, req: s::WriteTextFileRequest, responder: Responder<s::WriteTextFileResponse>) {
        let request = EditorRequest::WriteText {
            path: req.path,
            content: req.content,
        };
        self.route(&req.session_id, request, move |answer| {
            let _ = match answer {
                Ok(_) => responder.respond(s::WriteTextFileResponse::default()),
                Err(e) => responder.respond_with_error(e),
            };
        });
    }

    /// Sends `request` to the main thread; `finish` gets the reply's text, or the error for the
    /// agent. The wait runs on a thread of its own, so the connection keeps going meanwhile.
    fn route(
        &self,
        session: &s::SessionId,
        request: EditorRequest,
        finish: impl FnOnce(Result<String, agent_client_protocol::Error>) + Send + 'static,
    ) {
        let Some(tools) = &self.tools else {
            finish(Err(agent_client_protocol::Error::method_not_found()));
            return;
        };
        let ours = self
            .shared
            .lock()
            .session
            .as_ref()
            .is_none_or(|sid| sid == session);
        if !ours {
            finish(Err(agent_client_protocol::Error::invalid_params().data(
                serde_json::json!(format!("unknown session {}", session.0)),
            )));
            return;
        }
        let timeout = mcp::Timeouts::default().for_request(&request);
        let (reply, answer) = mpsc::sync_channel::<ToolReply>(1);
        let call = ToolCall {
            call: mcp::next_call_id(),
            req: request,
            reply,
        };
        if tools.try_send(call).is_err() {
            finish(Err(fs_error("the editor is not taking requests")));
            return;
        }
        let spawned = thread::Builder::new()
            .name("lemma-agent-fs".into())
            .spawn(move || {
                let result = match answer.recv_timeout(timeout) {
                    Ok(reply) => {
                        let text = reply
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                crate::ToolContent::Text(t) => Some(t.as_str()),
                                crate::ToolContent::Png(_) => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        if reply.is_error {
                            Err(fs_error(&text))
                        } else {
                            Ok(text)
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => Err(fs_error(&format!(
                        "the editor did not answer within {} s",
                        timeout.as_secs()
                    ))),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        Err(fs_error("the editor dropped the request"))
                    }
                };
                finish(result);
            });
        if let Err(e) = spawned {
            tracing::warn!("cannot start an fs request thread: {e}");
        }
    }
}

/// The JSON-RPC error of a failed `fs/*` request: the editor's message.
fn fs_error(message: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::new(-32603, message)
}

/// `_meta` of `session/new` (§10.4).
fn session_meta(append: &str) -> s::Meta {
    let meta = serde_json::json!({
        "claudeCode": {"options": {
            "disallowedTools": ["Edit", "Write", "NotebookEdit"],
            "allowedTools": ["mcp__lemma__*"]
        }},
        "systemPrompt": {"append": append}
    });
    match meta {
        serde_json::Value::Object(map) => map,
        _ => unreachable!("a JSON object literal"),
    }
}

/// The ACP content of a prompt block. `images`: the agent accepts `image` blocks; without that
/// capability an image becomes a `resource_link` to its file (a note for a pasted one).
fn content_block(block: PromptBlock, images: bool) -> s::ContentBlock {
    match block {
        PromptBlock::Text(text) => s::ContentBlock::Text(s::TextContent::new(text)),
        PromptBlock::Resource { uri, mime, text } => {
            let mut contents = s::TextResourceContents::new(text, uri);
            if !mime.is_empty() {
                contents = contents.mime_type(mime);
            }
            s::ContentBlock::Resource(s::EmbeddedResource::new(
                s::EmbeddedResourceResource::TextResourceContents(contents),
            ))
        }
        PromptBlock::Image { uri, mime, data } if images => {
            let mut image = s::ImageContent::new(mcp::base64(&data), mime);
            if !uri.is_empty() {
                image = image.uri(uri);
            }
            s::ContentBlock::Image(image)
        }
        PromptBlock::Image { uri, .. } if uri.is_empty() => {
            s::ContentBlock::Text(s::TextContent::new(
                "[The user pasted an image, but this agent cannot receive images.]",
            ))
        }
        PromptBlock::Image { uri, mime, data } | PromptBlock::Document { uri, mime, data } => {
            s::ContentBlock::ResourceLink(resource_link(uri, mime, data.len()))
        }
    }
}

/// What to tell the user when `blocks` carry images for an agent that does not accept them.
fn no_images_notice(blocks: &[PromptBlock]) -> Option<&'static str> {
    let (mut files, mut pasted) = (false, false);
    for block in blocks {
        if let PromptBlock::Image { uri, .. } = block {
            if uri.is_empty() {
                pasted = true;
            } else {
                files = true;
            }
        }
    }
    match (files, pasted) {
        (false, false) => None,
        (true, false) => Some(
            "This agent does not accept images: it was given the paths of the attached image \
             files instead.",
        ),
        (false, true) => Some("This agent does not accept images: the pasted image was not sent."),
        (true, true) => Some(
            "This agent does not accept images: it was given the paths of the attached image \
             files instead, and the pasted image was not sent.",
        ),
    }
}

/// A `resource_link` to the attached file at `uri`; `size` 0: unknown (a file sent without its
/// bytes).
fn resource_link(uri: String, mime: String, size: usize) -> s::ResourceLink {
    let mut link = s::ResourceLink::new(PromptBlock::file_name(&uri), uri);
    if !mime.is_empty() {
        link = link.mime_type(mime);
    }
    link.size(i64::try_from(size).ok().filter(|&n| n > 0))
}

// ============================================================================== shared state

/// State shared by the command loop, the SDK's handlers and the request tasks. All run on the
/// `lemma-agent` thread; the mutex only satisfies the SDK's `Send` bounds.
struct Shared {
    events: async_channel::Sender<AgentEvent>,
    /// Set by the wait thread when the agent exited.
    exited: Arc<AtomicBool>,
    /// Which modes Lemma offers ([`AcpFlavor::offers_mode`]).
    flavor: AcpFlavor,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    session: Option<s::SessionId>,
    /// `Ready` was sent; until then events wait in `pre_ready`.
    ready: bool,
    pre_ready: Vec<AgentEvent>,
    turn: Option<Turn>,
    next_turn: u64,
    permissions: BTreeMap<u64, PendingPermission>,
    next_permission: u64,
    mode: String,
    /// Bumped when an update from the agent changes the mode: a `session/set_mode` sent before
    /// is then not confirmed (the update is newer). Answers to our own requests do not bump it.
    mode_generation: u64,
    /// A switch back to `fallback_mode` is in flight (or failed): do not ask again.
    reverting_mode: bool,
    /// The offered mode a session in a mode Lemma does not offer is switched to
    /// ([`fallback_mode`]); `None` when there is none.
    fallback_mode: Option<String>,
    /// Lemma's instructions, until the first prompt carries them ([`AcpFlavor::Generic`]).
    instructions: Option<String>,
    options: Vec<ConfigOpt>,
    boolean_options: HashSet<String>,
    agent: String,
    modes: Vec<(String, String)>,
    /// Titles of the turn's tool calls, for permission requests without a title.
    tool_titles: HashMap<String, String>,
}

struct Turn {
    seq: u64,
    cancel_requested: bool,
}

struct PendingPermission {
    responder: Responder<s::RequestPermissionResponse>,
    option_ids: Vec<String>,
    /// Never sent on: dropping it (the request was settled) ends the withdrawal watch.
    _settled: async_channel::Sender<()>,
}

impl Shared {
    fn new(
        events: async_channel::Sender<AgentEvent>,
        exited: Arc<AtomicBool>,
        flavor: AcpFlavor,
    ) -> Self {
        Self {
            events,
            exited,
            flavor,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Sends now, or after `Ready` while starting.
    fn emit_locked(&self, st: &mut State, event: AgentEvent) {
        if st.ready {
            let _ = self.events.try_send(event);
        } else {
            st.pre_ready.push(event);
        }
    }

    fn emit(&self, event: AgentEvent) {
        let mut st = self.lock();
        self.emit_locked(&mut st, event);
    }

    /// True once the agent is gone (its process exited).
    fn dying(&self, e: &agent_client_protocol::Error) -> bool {
        agent_client_protocol::is_incoming_transport_closed(e) || self.exited.load(Ordering::SeqCst)
    }

    /// Reports a failed request as an `Error`, unless the agent is going away (the death path
    /// reports that).
    fn request_failed(&self, e: &agent_client_protocol::Error, what: &str) {
        if !self.dying(e) {
            self.emit(AgentEvent::Error(format!("{what}: {}", describe(e))));
        }
    }

    /// Records the `session/new` response. Runs in the response's ordered callback, so before
    /// any later message from the agent is dispatched.
    fn begin_session(&self, agent: &str, new: &s::NewSessionResponse) {
        let mut st = self.lock();
        st.session = Some(new.session_id.clone());
        agent.clone_into(&mut st.agent);
        st.modes = new
            .modes
            .as_ref()
            .map(|m| {
                m.available_modes
                    .iter()
                    .filter(|m| self.flavor.offers_mode(&m.id.0))
                    .map(|m| (m.id.0.to_string(), m.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        st.fallback_mode = fallback_mode(self.flavor, &st.modes);
        st.mode = new
            .modes
            .as_ref()
            .map(|m| m.current_mode_id.0.to_string())
            .unwrap_or_default();
        let (options, booleans) = convert_options(
            new.config_options.as_deref().unwrap_or_default(),
            self.flavor,
        );
        st.options = options;
        st.boolean_options = booleans;
    }

    fn become_ready(self: &Arc<Self>, started: &Started, cx: &ConnectionTo<Agent>) {
        let mut st = self.lock();
        let ready = AgentEvent::Ready {
            session: started.session_id.0.to_string(),
            agent: st.agent.clone(),
            modes: st.modes.clone(),
            mode: st.mode.clone(),
            options: st.options.clone(),
        };
        let _ = self.events.try_send(ready);
        st.ready = true;
        for event in std::mem::take(&mut st.pre_ready) {
            // `Ready` already carries the current mode and options.
            if !matches!(
                event,
                AgentEvent::ModeChanged(_) | AgentEvent::ConfigChanged(_)
            ) {
                let _ = self.events.try_send(event);
            }
        }
        // An update during start-up may have switched the agent to a mode Lemma does not offer;
        // that was only recorded, since the switch back (and its `Notice`) waits for `Ready`.
        let mode = st.mode.clone();
        self.enforce_offered_mode(&mut st, &mode, cx);
    }

    /// Starts a turn unless one runs; its sequence number.
    fn begin_turn(&self) -> Option<u64> {
        let mut st = self.lock();
        if st.turn.is_some() {
            return None;
        }
        st.next_turn += 1;
        let seq = st.next_turn;
        st.turn = Some(Turn {
            seq,
            cancel_requested: false,
        });
        Some(seq)
    }

    /// Marks the running turn as cancelled; false if none runs.
    fn request_cancel(&self) -> bool {
        let mut st = self.lock();
        match st.turn.as_mut() {
            Some(turn) => {
                turn.cancel_requested = true;
                true
            }
            None => false,
        }
    }

    fn finish_turn(
        &self,
        seq: u64,
        result: Result<s::PromptResponse, agent_client_protocol::Error>,
    ) {
        let mut st = self.lock();
        let Some(turn) = st.turn.as_ref().filter(|t| t.seq == seq) else {
            return;
        };
        let cancel_requested = turn.cancel_requested;
        let reason = match result {
            Ok(response) => stop_reason(response.stop_reason),
            Err(e) => {
                if self.dying(&e) {
                    // The death path ends the turn after the process is gone.
                    return;
                }
                if cancel_requested {
                    StopReason::Cancelled
                } else {
                    self.emit_locked(
                        &mut st,
                        AgentEvent::Error(format!("The agent failed: {}", describe(&e))),
                    );
                    StopReason::EndTurn
                }
            }
        };
        st.turn = None;
        st.tool_titles.clear();
        self.emit_locked(&mut st, AgentEvent::TurnEnded(reason));
    }

    fn on_permission(
        self: &Arc<Self>,
        req: s::RequestPermissionRequest,
        responder: Responder<s::RequestPermissionResponse>,
        cx: &ConnectionTo<Agent>,
    ) {
        let mut st = self.lock();
        let other_session = st
            .session
            .as_ref()
            .is_some_and(|sid| *sid != req.session_id);
        let cancelling = st.turn.as_ref().is_some_and(|t| t.cancel_requested);
        if other_session || cancelling {
            // Protocol: after session/cancel every permission request is answered Cancelled.
            let _ = responder.respond(s::RequestPermissionResponse::new(
                s::RequestPermissionOutcome::Cancelled,
            ));
            return;
        }
        let id = st.next_permission;
        st.next_permission += 1;
        let tool_id = req.tool_call.tool_call_id.0.to_string();
        let title = req
            .tool_call
            .fields
            .title
            .clone()
            .or_else(|| st.tool_titles.get(&tool_id).cloned())
            .unwrap_or_else(|| "The agent asks for permission".to_owned());
        let options: Vec<PermOption> = req
            .options
            .iter()
            .filter(|o| !ELEVATING_OPTIONS.contains(&&*o.option_id.0))
            .map(|o| PermOption {
                id: o.option_id.0.to_string(),
                name: o.name.clone(),
                kind: permission_kind(o.kind).to_owned(),
            })
            .collect();
        let withdrawn = responder.cancellation();
        let (settled, settled_rx) = async_channel::bounded::<()>(1);
        st.permissions.insert(
            id,
            PendingPermission {
                responder,
                option_ids: options.iter().map(|o| o.id.clone()).collect(),
                _settled: settled,
            },
        );
        self.emit_locked(
            &mut st,
            AgentEvent::Permission {
                request: id,
                title,
                options,
            },
        );
        drop(st);
        // The agent may withdraw the request with `$/cancel_request` (the adapter does when the
        // tool use is aborted). The watch ends when the request is settled either way.
        let shared = self.clone();
        if let Err(e) = cx.spawn(async move {
            let settled = settled_rx.recv();
            if let Race::A(()) =
                race3(withdrawn.cancelled(), settled, std::future::pending::<()>()).await
            {
                shared.permission_withdrawn(id);
            }
            Ok(())
        }) {
            tracing::warn!("cannot watch permission request {id} for its withdrawal: {e}");
        }
    }

    /// The agent withdrew permission request `request`: answer it with the request-cancelled
    /// error (it gets no other answer) and tell the UI, unless it was answered already.
    fn permission_withdrawn(&self, request: u64) {
        let mut st = self.lock();
        let Some(pending) = st.permissions.remove(&request) else {
            return;
        };
        let _ = pending
            .responder
            .respond_with_error(agent_client_protocol::Error::request_cancelled());
        self.emit_locked(&mut st, AgentEvent::PermissionWithdrawn { request });
    }

    fn answer_permission(&self, request: u64, option_id: Option<String>) {
        let mut st = self.lock();
        let Some(pending) = st.permissions.remove(&request) else {
            tracing::debug!(
                request,
                "answer to a permission request that is no longer pending"
            );
            return;
        };
        let outcome = match option_id {
            Some(id) if pending.option_ids.contains(&id) => {
                s::RequestPermissionOutcome::Selected(s::SelectedPermissionOutcome::new(id))
            }
            Some(id) => {
                self.emit_locked(
                    &mut st,
                    AgentEvent::Error(format!(
                        "\"{id}\" is not one of the offered permission options; the request was cancelled."
                    )),
                );
                s::RequestPermissionOutcome::Cancelled
            }
            None => s::RequestPermissionOutcome::Cancelled,
        };
        let _ = pending
            .responder
            .respond(s::RequestPermissionResponse::new(outcome));
    }

    fn answer_all_cancelled(&self) {
        let pending = std::mem::take(&mut self.lock().permissions);
        for (_, p) in pending {
            let _ = p.responder.respond(s::RequestPermissionResponse::new(
                s::RequestPermissionOutcome::Cancelled,
            ));
        }
    }

    fn mode_generation(&self) -> u64 {
        self.lock().mode_generation
    }

    /// Records the current mode without an event (start-up, before `Ready`), unless an update
    /// changed the mode since `generation_before` (it is newer).
    fn set_mode_silently(&self, mode: &str, generation_before: u64) {
        let mut st = self.lock();
        if st.mode_generation != generation_before {
            return;
        }
        mode.clone_into(&mut st.mode);
        if let Some(option) = st.options.iter_mut().find(|o| o.id == "mode") {
            mode.clone_into(&mut option.current);
        }
    }

    /// Records `mode` and sends `ModeChanged` (with `force` also when it is the current mode);
    /// false when nothing changed.
    fn mode_known_locked(&self, st: &mut State, mode: &str, force: bool) -> bool {
        if st.mode == mode && !force {
            return false;
        }
        mode.clone_into(&mut st.mode);
        if self.flavor.offers_mode(mode) {
            st.reverting_mode = false;
        }
        if let Some(option) = st.options.iter_mut().find(|o| o.id == "mode") {
            mode.clone_into(&mut option.current);
        }
        self.emit_locked(st, AgentEvent::ModeChanged(mode.to_owned()));
        true
    }

    /// An update from the agent reports `mode`. Unlike the answers to our own requests (which
    /// arrive in order), it supersedes the confirmations of requests sent before it.
    fn mode_reported_locked(&self, st: &mut State, mode: &str) {
        if self.mode_known_locked(st, mode, false) {
            st.mode_generation += 1;
        }
    }

    /// `session/set_mode` succeeded: confirm with `ModeChanged` unless an update already did.
    fn mode_confirmed(&self, mode: &str, generation_before: u64) {
        let mut st = self.lock();
        if st.mode_generation == generation_before {
            self.mode_known_locked(&mut st, mode, true);
        }
    }

    fn config_confirmed(&self, options: &[s::SessionConfigOption]) {
        let mut st = self.lock();
        let (options, booleans) = convert_options(options, self.flavor);
        st.boolean_options = booleans;
        st.options.clone_from(&options);
        self.emit_locked(&mut st, AgentEvent::ConfigChanged(options.clone()));
        if let Some(mode) = options.iter().find(|o| o.id == "mode") {
            let mode = mode.current.clone();
            self.mode_known_locked(&mut st, &mode, false);
        }
    }

    fn is_boolean_option(&self, id: &str) -> bool {
        self.lock().boolean_options.contains(id)
    }

    fn on_update(self: &Arc<Self>, n: s::SessionNotification, cx: &ConnectionTo<Agent>) {
        let mut st = self.lock();
        if st.session.as_ref().is_some_and(|sid| *sid != n.session_id) {
            tracing::debug!("update for another session ignored");
            return;
        }
        let event = match n.update {
            s::SessionUpdate::AgentMessageChunk(c) => {
                content_text(c.content).map(AgentEvent::MessageChunk)
            }
            s::SessionUpdate::AgentThoughtChunk(c) => {
                content_text(c.content).map(AgentEvent::ThoughtChunk)
            }
            s::SessionUpdate::UserMessageChunk(_) => None,
            s::SessionUpdate::ToolCall(tc) => {
                let id = tc.tool_call_id.0.to_string();
                st.tool_titles.insert(id.clone(), tc.title.clone());
                Some(AgentEvent::ToolCall {
                    id,
                    title: tc.title,
                    kind: tool_kind(tc.kind).to_owned(),
                    status: tool_status(tc.status),
                })
            }
            s::SessionUpdate::ToolCallUpdate(u) => {
                let id = u.tool_call_id.0.to_string();
                if let Some(title) = &u.fields.title {
                    st.tool_titles.insert(id.clone(), title.clone());
                }
                let status = u.fields.status.map(tool_status);
                (u.fields.title.is_some() || status.is_some()).then_some(AgentEvent::ToolUpdate {
                    id,
                    title: u.fields.title,
                    status,
                })
            }
            s::SessionUpdate::Plan(plan) => Some(AgentEvent::Plan(
                plan.entries
                    .into_iter()
                    .map(|e| (e.content, plan_status(&e.status).to_owned()))
                    .collect(),
            )),
            s::SessionUpdate::AvailableCommandsUpdate(u) => Some(AgentEvent::Commands(
                u.available_commands
                    .into_iter()
                    .map(|c| (c.name, c.description))
                    .collect(),
            )),
            s::SessionUpdate::CurrentModeUpdate(u) => {
                let mode = u.current_mode_id.0.to_string();
                self.mode_reported_locked(&mut st, &mode);
                self.enforce_offered_mode(&mut st, &mode, cx);
                None
            }
            s::SessionUpdate::ConfigOptionUpdate(u) => {
                let (options, booleans) = convert_options(&u.config_options, self.flavor);
                st.boolean_options = booleans;
                let mode = options
                    .iter()
                    .find(|o| o.id == "mode")
                    .map(|o| o.current.clone());
                if options != st.options {
                    st.options.clone_from(&options);
                    self.emit_locked(&mut st, AgentEvent::ConfigChanged(options));
                }
                if let Some(mode) = mode {
                    self.mode_reported_locked(&mut st, &mode);
                    self.enforce_offered_mode(&mut st, &mode, cx);
                }
                None
            }
            s::SessionUpdate::UsageUpdate(u) => Some(AgentEvent::Usage {
                used: u.used,
                size: u.size,
            }),
            s::SessionUpdate::Notice(notice) => {
                Some(AgentEvent::Notice(match notice.description {
                    Some(d) if !d.is_empty() => format!("{}: {d}", notice.title),
                    _ => notice.title,
                }))
            }
            other => {
                tracing::debug!("ignoring session update {}", update_name(&other));
                None
            }
        };
        if let Some(event) = event {
            self.emit_locked(&mut st, event);
        }
    }

    /// The agent switched to a mode Lemma does not offer: switch back to the fallback mode
    /// (`default`).
    fn enforce_offered_mode(
        self: &Arc<Self>,
        st: &mut State,
        mode: &str,
        cx: &ConnectionTo<Agent>,
    ) {
        let Some(sid) = st.session.clone() else {
            return;
        };
        if !st.ready || st.reverting_mode || mode.is_empty() || self.flavor.offers_mode(mode) {
            return;
        }
        st.reverting_mode = true;
        let Some(fallback) = st.fallback_mode.clone() else {
            self.emit_locked(
                st,
                AgentEvent::Notice(format!(
                    "The agent switched to the mode \"{mode}\", which Lemma does not offer, and \
                     has no other mode to switch back to."
                )),
            );
            return;
        };
        self.emit_locked(
            st,
            AgentEvent::Notice(format!(
                "The agent switched to the mode \"{mode}\", which Lemma does not offer; switching back to \"{fallback}\"."
            )),
        );
        let before = st.mode_generation;
        let sent = cx.send_request(s::SetSessionModeRequest::new(sid, fallback.clone()));
        let shared = self.clone();
        if let Err(e) = cx.spawn(async move {
            match sent.block_task().await {
                // Confirmed with `ModeChanged` unless an update already reported the switch.
                Ok(_) => shared.mode_confirmed(&fallback, before),
                Err(e) => {
                    shared.request_failed(
                        &e,
                        &format!("Could not switch the agent back to \"{fallback}\""),
                    );
                }
            }
            Ok(())
        }) {
            tracing::warn!("cannot switch the agent back to an offered mode: {e}");
        }
    }
}

fn update_name(update: &s::SessionUpdate) -> &'static str {
    match update {
        s::SessionUpdate::SessionInfoUpdate(_) => "session_info_update",
        s::SessionUpdate::CompactionUpdate(_) => "compaction_update",
        s::SessionUpdate::CompactionSummaryChunk(_) => "compaction_summary_chunk",
        _ => "(unknown)",
    }
}

/// The mode a session in a mode Lemma does not offer is switched to, from the offered `modes`:
/// `default`; for other agents than Claude Code, without one, the first offered mode.
fn fallback_mode(flavor: AcpFlavor, modes: &[(String, String)]) -> Option<String> {
    let ids = || modes.iter().map(|(id, _)| id);
    ids()
        .find(|id| *id == "default")
        .or_else(|| ids().next().filter(|_| flavor == AcpFlavor::Generic))
        .cloned()
}

/// `ConfigOpt`s (the `mode` option limited to the modes `flavor` offers) and the ids of boolean
/// options.
fn convert_options(
    options: &[s::SessionConfigOption],
    flavor: AcpFlavor,
) -> (Vec<ConfigOpt>, HashSet<String>) {
    let mut out = Vec::new();
    let mut booleans = HashSet::new();
    for option in options {
        let id = option.id.0.to_string();
        let (current, mut values) = match &option.kind {
            s::SessionConfigKind::Select(select) => {
                let mut values = Vec::new();
                match &select.options {
                    s::SessionConfigSelectOptions::Ungrouped(list) => {
                        values.extend(list.iter().map(|o| (o.value.0.to_string(), o.name.clone())));
                    }
                    s::SessionConfigSelectOptions::Grouped(groups) => {
                        for group in groups {
                            values.extend(
                                group
                                    .options
                                    .iter()
                                    .map(|o| (o.value.0.to_string(), o.name.clone())),
                            );
                        }
                    }
                    _ => {}
                }
                (select.current_value.0.to_string(), values)
            }
            s::SessionConfigKind::Boolean(b) => {
                booleans.insert(id.clone());
                (
                    b.current_value.to_string(),
                    vec![
                        ("true".to_owned(), "On".to_owned()),
                        ("false".to_owned(), "Off".to_owned()),
                    ],
                )
            }
            _ => continue,
        };
        if id == "mode" {
            values.retain(|(v, _)| flavor.offers_mode(v));
        }
        out.push(ConfigOpt {
            id,
            name: option.name.clone(),
            current,
            values,
        });
    }
    (out, booleans)
}

fn content_text(block: s::ContentBlock) -> Option<String> {
    match block {
        s::ContentBlock::Text(t) => Some(t.text),
        s::ContentBlock::ResourceLink(link) => Some(format!(
            "[{}]({})",
            link.title.unwrap_or(link.name),
            link.uri
        )),
        s::ContentBlock::Image(_) => Some("[image]".to_owned()),
        s::ContentBlock::Audio(_) => Some("[audio]".to_owned()),
        s::ContentBlock::Resource(r) => match r.resource {
            s::EmbeddedResourceResource::TextResourceContents(t) => Some(t.text),
            s::EmbeddedResourceResource::BlobResourceContents(b) => Some(format!("[{}]", b.uri)),
            _ => None,
        },
        _ => None,
    }
}

fn stop_reason(reason: s::StopReason) -> StopReason {
    match reason {
        s::StopReason::MaxTokens => StopReason::MaxTokens,
        s::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        s::StopReason::Refusal => StopReason::Refusal,
        s::StopReason::Cancelled => StopReason::Cancelled,
        _ => StopReason::EndTurn,
    }
}

fn tool_status(status: s::ToolCallStatus) -> ToolStatus {
    match status {
        s::ToolCallStatus::InProgress => ToolStatus::InProgress,
        s::ToolCallStatus::Completed => ToolStatus::Completed,
        s::ToolCallStatus::Failed => ToolStatus::Failed,
        _ => ToolStatus::Pending,
    }
}

fn tool_kind(kind: s::ToolKind) -> &'static str {
    match kind {
        s::ToolKind::Read => "read",
        s::ToolKind::Edit => "edit",
        s::ToolKind::Delete => "delete",
        s::ToolKind::Move => "move",
        s::ToolKind::Search => "search",
        s::ToolKind::Execute => "execute",
        s::ToolKind::Think => "think",
        s::ToolKind::Fetch => "fetch",
        s::ToolKind::SwitchMode => "switch_mode",
        _ => "other",
    }
}

fn plan_status(status: &s::PlanEntryStatus) -> &'static str {
    match status {
        s::PlanEntryStatus::InProgress => "in_progress",
        s::PlanEntryStatus::Completed => "completed",
        _ => "pending",
    }
}

fn permission_kind(kind: s::PermissionOptionKind) -> &'static str {
    match kind {
        s::PermissionOptionKind::AllowOnce => "allow_once",
        s::PermissionOptionKind::AllowAlways => "allow_always",
        s::PermissionOptionKind::RejectOnce => "reject_once",
        s::PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

// ================================================================================== races

enum Race<A, B, C, D> {
    A(A),
    B(B),
    C(C),
    D(D),
}

/// Polls the futures in order until one is ready.
async fn race4<A, B, C, D>(
    a: A,
    b: B,
    c: C,
    d: D,
) -> Race<A::Output, B::Output, C::Output, D::Output>
where
    A: Future,
    B: Future,
    C: Future,
    D: Future,
{
    let (mut a, mut b, mut c, mut d) = (pin!(a), pin!(b), pin!(c), pin!(d));
    poll_fn(|cx: &mut Context<'_>| {
        if let Poll::Ready(v) = a.as_mut().poll(cx) {
            return Poll::Ready(Race::A(v));
        }
        if let Poll::Ready(v) = b.as_mut().poll(cx) {
            return Poll::Ready(Race::B(v));
        }
        if let Poll::Ready(v) = c.as_mut().poll(cx) {
            return Poll::Ready(Race::C(v));
        }
        if let Poll::Ready(v) = d.as_mut().poll(cx) {
            return Poll::Ready(Race::D(v));
        }
        Poll::Pending
    })
    .await
}

async fn race3<A, B, C>(a: A, b: B, c: C) -> Race<A::Output, B::Output, C::Output, ()>
where
    A: Future,
    B: Future,
    C: Future,
{
    race4(a, b, c, std::future::pending::<()>()).await
}

// ============================================================================= the transport

/// How many of the agent's lines the transport takes in a row before the connection's other
/// tasks get their turn.
const LINES_PER_POLL: usize = 64;

/// The incoming half of the SDK's line transport: the lines the `lemma-agent-out` thread read.
///
/// Everything runs in one `block_on`, and the SDK's actors keep going while their input is
/// ready: without a pause, a backlog of agent output (a burst of updates) would be parsed and
/// dispatched to the end before the command loop ran again, and `Cancel`, `Shutdown` and
/// permission answers would wait for it. So after [`LINES_PER_POLL`] lines in a row this stream
/// is `Pending` once (waking itself at once), which ends the round.
struct CooperativeLines {
    rx: Pin<Box<async_channel::Receiver<io::Result<String>>>>,
    /// Lines handed out since the stream was last `Pending`.
    streak: usize,
}

impl CooperativeLines {
    fn new(rx: async_channel::Receiver<io::Result<String>>) -> Self {
        Self {
            rx: Box::pin(rx),
            streak: 0,
        }
    }
}

impl futures_core::Stream for CooperativeLines {
    type Item = io::Result<String>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.streak >= LINES_PER_POLL {
            this.streak = 0;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let next = this.rx.as_mut().poll_next(cx);
        match next {
            Poll::Ready(_) => this.streak += 1,
            Poll::Pending => this.streak = 0,
        }
        next
    }
}

/// The outgoing half of the SDK's line transport: lines go to the `lemma-agent-in` thread.
/// Closing it (or dropping it) closes the agent's stdin once the queued lines are written.
struct LineSink {
    tx: Option<mpsc::Sender<String>>,
}

impl LineSink {
    fn closed() -> io::Error {
        io::Error::new(io::ErrorKind::BrokenPipe, "the agent's stdin is closed")
    }
}

impl futures_sink::Sink<String> for LineSink {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(if self.tx.is_some() {
            Ok(())
        } else {
            Err(Self::closed())
        })
    }

    fn start_send(self: Pin<&mut Self>, line: String) -> io::Result<()> {
        match &self.tx {
            Some(tx) => tx.send(line).map_err(|_| Self::closed()),
            None => Err(Self::closed()),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.tx = None;
        Poll::Ready(Ok(()))
    }
}

/// `raw_log`: every JSON-RPC line (`>>` to the agent, `<<` from it) and stderr line (`!!`).
/// Debug only: the file can hold account details. It is created with mode 0600.
#[derive(Clone)]
struct RawLog(Arc<Mutex<File>>);

impl RawLog {
    fn create(path: &std::path::Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(Self(Arc::new(Mutex::new(file))))
    }

    fn line(&self, direction: &str, line: &str) {
        let mut file = lock(&self.0);
        let _ = writeln!(file, "{direction} {line}");
    }
}

fn read_stdout(
    stdout: ChildStdout,
    incoming: &async_channel::Sender<io::Result<String>>,
    log: Option<&RawLog>,
) {
    let mut reader = BufReader::with_capacity(64 * 1024, stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return,
            Ok(_) => {
                while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    buf.pop();
                }
                let line = String::from_utf8(std::mem::take(&mut buf))
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(log) = log {
                    log.line("<<", &line);
                }
                if incoming.send_blocking(Ok(line)).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                let _ = incoming.send_blocking(Err(e));
                return;
            }
        }
    }
}

fn write_stdin(stdin: ChildStdin, lines: &mpsc::Receiver<String>, log: Option<&RawLog>) {
    let mut out = BufWriter::new(stdin);
    for line in lines {
        if let Some(log) = log {
            log.line(">>", &line);
        }
        let written = out
            .write_all(line.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
        if let Err(e) = written {
            tracing::debug!("the agent's stdin is closed: {e}");
            return;
        }
    }
    // The connection closed its sink: dropping `out` closes the agent's stdin.
}

fn read_stderr(mut stderr: ChildStderr, ring: &Mutex<StderrRing>, log: Option<&RawLog>) {
    let mut buf = [0u8; 8192];
    let mut partial = Vec::new();
    loop {
        let n = match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        lock(ring).push(&buf[..n]);
        // Logging (tracing at trace level, the raw log) works on whole lines.
        partial.extend_from_slice(&buf[..n]);
        while let Some(pos) = partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = partial.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end();
            tracing::trace!(target: "lemma_agent::stderr", "{line}");
            if let Some(log) = log {
                log.line("!!", line);
            }
        }
        if partial.len() > STDERR_RING_BYTES {
            partial.clear();
        }
    }
    if !partial.is_empty()
        && let Some(log) = log
    {
        log.line("!!", String::from_utf8_lossy(&partial).trim_end());
    }
}

/// The last [`STDERR_RING_BYTES`] bytes of the agent's stderr.
#[derive(Default)]
struct StderrRing {
    bytes: VecDeque<u8>,
    dropped: bool,
}

impl StderrRing {
    fn push(&mut self, data: &[u8]) {
        let data = if data.len() > STDERR_RING_BYTES {
            self.dropped = true;
            &data[data.len() - STDERR_RING_BYTES..]
        } else {
            data
        };
        let overflow = (self.bytes.len() + data.len()).saturating_sub(STDERR_RING_BYTES);
        if overflow > 0 {
            self.dropped = true;
            self.bytes.drain(..overflow);
        }
        self.bytes.extend(data);
    }

    /// The captured text (lossy UTF-8, trailing whitespace trimmed); a leading `…` line when
    /// earlier output was dropped.
    fn text(&mut self) -> String {
        let text = String::from_utf8_lossy(self.bytes.make_contiguous());
        let text = text.trim_end();
        if text.is_empty() {
            String::new()
        } else if self.dropped {
            format!("…\n{text}")
        } else {
            text.to_owned()
        }
    }
}

// ============================================================================== the process

/// What the wait thread tells the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcSignal {
    /// The agent exited.
    Exited,
    /// [`DRAIN_WAIT`] has passed since then.
    Deadline,
}

/// The agent process (a process-group leader) and its `lemma-agent-wait` thread.
///
/// The wait thread waits with `WNOWAIT`, so the exited leader stays a zombie until it is allowed
/// to reap it: until then its pid, which is also the process group id, cannot be reused, and
/// signalling the group can never hit an unrelated process.
///
/// On Windows the group is a job object ([`lemma_core::process::Job`]), which names no pid, and
/// the wait thread holds the leader's process handle until it may "reap" it by closing it.
struct AgentProcess {
    pid: Pid,
    #[cfg(windows)]
    job: Arc<lemma_core::process::Job>,
    shared: Arc<(Mutex<ProcState>, Condvar)>,
    exited_flag: Arc<AtomicBool>,
    waiter: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct ProcState {
    /// `Some(code)` once the leader exited (`code` is `None` when a signal killed it).
    exited: Option<Option<i32>>,
    reap_allowed: bool,
    reaped: bool,
}

/// A process id.
#[cfg(unix)]
type Pid = libc::pid_t;
#[cfg(windows)]
type Pid = u32;

/// The agent process as [`spawn`] hands it to [`AgentProcess::watch`]: its pid, or on Windows
/// the process itself and its job.
#[cfg(unix)]
type Leader = Pid;
#[cfg(windows)]
struct Leader {
    child: std::process::Child,
    job: lemma_core::process::Job,
}

/// The exit code of processes that [`AgentProcess`] ends on Windows.
#[cfg(windows)]
const KILLED_EXIT_CODE: u32 = 1;

impl AgentProcess {
    fn watch(leader: Leader, exit_tx: async_channel::Sender<ProcSignal>) -> io::Result<Self> {
        #[cfg(unix)]
        let pid = leader;
        #[cfg(windows)]
        let (pid, job, mut child) = (leader.child.id(), Arc::new(leader.job), leader.child);
        let shared: Arc<(Mutex<ProcState>, Condvar)> = Arc::default();
        let exited_flag = Arc::new(AtomicBool::new(false));
        let waiter = {
            let shared = shared.clone();
            let flag = exited_flag.clone();
            named("lemma-agent-wait", move || {
                #[cfg(unix)]
                let code = wait_exited(pid);
                #[cfg(windows)]
                let code = child.wait().ok().and_then(|status| status.code());
                flag.store(true, Ordering::SeqCst);
                {
                    let mut st = lock(&shared.0);
                    st.exited = Some(code);
                    shared.1.notify_all();
                }
                let _ = exit_tx.try_send(ProcSignal::Exited);
                let mut st = lock(&shared.0);
                let deadline = Instant::now() + DRAIN_WAIT;
                while !st.reap_allowed && Instant::now() < deadline {
                    st = shared
                        .1
                        .wait_timeout(st, deadline.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                let _ = exit_tx.try_send(ProcSignal::Deadline);
                drop(exit_tx);
                while !st.reap_allowed {
                    st = shared.1.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                #[cfg(unix)]
                reap(pid);
                // Closing the process handle lets Windows reuse the pid.
                #[cfg(windows)]
                drop(child);
                st.reaped = true;
                shared.1.notify_all();
            })?
        };
        Ok(Self {
            pid,
            #[cfg(windows)]
            job,
            shared,
            exited_flag,
            waiter: Some(waiter),
        })
    }

    fn exited_flag(&self) -> Arc<AtomicBool> {
        self.exited_flag.clone()
    }

    fn has_exited(&self) -> bool {
        lock(&self.shared.0).exited.is_some()
    }

    /// Waits up to `timeout` for the leader to exit.
    fn wait_exit(&self, timeout: Duration) -> Option<Option<i32>> {
        let deadline = Instant::now() + timeout;
        let mut st = lock(&self.shared.0);
        loop {
            if let Some(code) = st.exited {
                return Some(code);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            st = self
                .shared
                .1
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Asks the whole process group to stop (SIGTERM). Windows has no such request for these
    /// programs: there the job ends at once.
    fn stop_group(&self) {
        #[cfg(unix)]
        self.signal_group(libc::SIGTERM);
        #[cfg(windows)]
        self.job.terminate(KILLED_EXIT_CODE);
    }

    /// Ends the whole process group (SIGKILL; on Windows the job).
    fn kill_group(&self) {
        #[cfg(unix)]
        self.signal_group(libc::SIGKILL);
        #[cfg(windows)]
        self.job.terminate(KILLED_EXIT_CODE);
    }

    /// Signals the whole process group, unless the leader was reaped already.
    #[cfg(unix)]
    fn signal_group(&self, signal: libc::c_int) {
        let st = lock(&self.shared.0);
        if !st.reaped {
            // SAFETY: plain syscall; the group id is the unreaped leader's pid, so it cannot
            // name a recycled group.
            unsafe {
                libc::killpg(self.pid, signal);
            }
        }
    }

    /// True while the leader runs or any other member of its group is alive.
    fn group_alive(&self) -> bool {
        #[cfg(unix)]
        return !self.has_exited() || group_has_members(self.pid);
        #[cfg(windows)]
        return !self.has_exited() || self.job.has_processes();
    }

    /// Ends the process group: `grace` for the leader to exit by itself, then SIGTERM to the
    /// group and [`TERM_GRACE`] for it to empty, then SIGKILL; then the leader is reaped. Returns
    /// the leader's exit code (`None` when a signal ended it).
    fn terminate(&mut self, grace: Duration) -> Option<i32> {
        self.wait_exit(grace);
        if self.group_alive() {
            self.stop_group();
            let deadline = Instant::now() + TERM_GRACE;
            while self.group_alive() && Instant::now() < deadline {
                if !self.has_exited() {
                    self.wait_exit(Duration::from_millis(10));
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        self.kill_group();
        let code = self.wait_exit(KILL_WAIT).flatten();
        {
            let mut st = lock(&self.shared.0);
            st.reap_allowed = true;
            self.shared.1.notify_all();
            let deadline = Instant::now() + KILL_WAIT;
            while !st.reaped && Instant::now() < deadline {
                st = self
                    .shared
                    .1
                    .wait_timeout(st, Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            if !st.reaped {
                tracing::warn!(
                    pid = self.pid,
                    "the agent process did not die; leaving it to its wait thread"
                );
                drop(st);
                drop(self.waiter.take());
                return code;
            }
        }
        if let Some(waiter) = self.waiter.take() {
            let _ = waiter.join();
        }
        code
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        if self.waiter.is_some() {
            // `terminate` did not run (`spawn` failed half-way, or a panic): kill the group and
            // let the wait thread reap the leader.
            self.kill_group();
            let mut st = lock(&self.shared.0);
            st.reap_allowed = true;
            self.shared.1.notify_all();
        }
    }
}

/// Waits for `pid` to exit without reaping it; its exit code (`None` for a signal).
#[cfg(unix)]
fn wait_exited(pid: libc::pid_t) -> Option<i32> {
    loop {
        // SAFETY: an all-zero siginfo_t is a valid value for waitid to fill in.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid writes only into `info`.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if r == 0 {
            return if info.si_code == libc::CLD_EXITED {
                // SAFETY: si_status is valid for SIGCHLD information.
                Some(unsafe { info.si_status() })
            } else {
                None
            };
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return None;
        }
    }
}

#[cfg(unix)]
fn reap(pid: libc::pid_t) {
    loop {
        let mut status = 0;
        // SAFETY: waitpid writes only into `status`.
        let r = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if r >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// True if a live (non-zombie) process is in process group `pgid`. Reads `/proc/*/stat`.
#[cfg(all(unix, not(target_os = "macos")))]
fn group_has_members(pgid: libc::pid_t) -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<libc::pid_t>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) state ppid pgrp …": comm may contain spaces and parentheses.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let state = fields.next();
        let pgrp = fields.nth(1).and_then(|p| p.parse::<libc::pid_t>().ok());
        if pgrp == Some(pgid) && !matches!(state, Some("Z" | "X")) {
            return true;
        }
    }
    false
}

/// True if a live (non-zombie) process is in process group `pgid`. Asks libproc: the group's
/// members (`proc_listpids(PROC_PGRP_ONLY)`), then each one's state (`PROC_PIDTBSDINFO`, which
/// also answers for zombies).
#[cfg(target_os = "macos")]
fn group_has_members(pgid: libc::pid_t) -> bool {
    /// `PROC_PGRP_ONLY` from `<sys/proc_info.h>`: list the members of one process group.
    const PROC_PGRP_ONLY: u32 = 2;
    let Ok(group) = u32::try_from(pgid) else {
        return false;
    };
    let mut pids: Vec<libc::pid_t> = vec![0; 64];
    let count = loop {
        let bytes = std::mem::size_of_val(pids.as_slice());
        let Ok(size) = libc::c_int::try_from(bytes) else {
            return false;
        };
        // SAFETY: proc_listpids writes at most `size` bytes into `pids`; it returns the number of
        // bytes written.
        let filled =
            unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), size) };
        let Ok(filled) = usize::try_from(filled) else {
            return false;
        };
        let count = filled / std::mem::size_of::<libc::pid_t>();
        // A full buffer may have cut the list short: ask again with a larger one.
        if count < pids.len() {
            break count;
        }
        pids.resize(pids.len() * 2, 0);
    };
    pids[..count]
        .iter()
        .any(|&pid| pid > 0 && is_live_process(pid))
}

/// True if `pid` exists and is not a zombie.
#[cfg(target_os = "macos")]
fn is_live_process(pid: libc::pid_t) -> bool {
    // SAFETY: an all-zero proc_bsdinfo is a valid value for proc_pidinfo to fill in.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let Ok(size) = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return false;
    };
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
    let filled =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    // Anything short of a full answer: the process is gone.
    filled == size && info.pbi_status != libc::SZOMB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_ring_keeps_the_last_64_kib() {
        let mut ring = StderrRing::default();
        ring.push(b"first line\n");
        assert_eq!(ring.text(), "first line");
        let big = vec![b'x'; STDERR_RING_BYTES];
        ring.push(&big);
        ring.push(b"\nlast line\n");
        let text = ring.text();
        assert!(text.starts_with("…\n"));
        assert!(text.ends_with("x\nlast line"));
        assert_eq!(ring.bytes.len(), STDERR_RING_BYTES);
        let mut huge = StderrRing::default();
        huge.push(&vec![b'y'; 3 * STDERR_RING_BYTES]);
        assert_eq!(huge.bytes.len(), STDERR_RING_BYTES);
        assert!(huge.dropped);
    }

    #[test]
    fn mode_option_is_limited_to_offered_modes() {
        let json = serde_json::json!([
            {"id": "mode", "name": "Mode", "type": "select", "currentValue": "default",
             "options": [{"value": "default", "name": "Manual"}, {"value": "acceptEdits", "name": "Accept edits"},
                         {"value": "plan", "name": "Plan"}, {"value": "bypassPermissions", "name": "Bypass"}]},
            {"id": "model", "name": "Model", "type": "select", "currentValue": "opus",
             "options": [{"group": "g", "name": "Claude", "options": [{"value": "opus", "name": "Opus"}, {"value": "haiku", "name": "Haiku"}]}]},
            {"id": "fast", "name": "Fast mode", "type": "boolean", "currentValue": false}
        ]);
        let options: Vec<s::SessionConfigOption> = serde_json::from_value(json).unwrap();
        let (converted, booleans) = convert_options(&options, AcpFlavor::Claude);
        assert_eq!(
            converted[0].values,
            [
                ("default".to_owned(), "Manual".to_owned()),
                ("plan".to_owned(), "Plan".to_owned())
            ]
        );
        assert_eq!(converted[1].values.len(), 2);
        assert_eq!(converted[1].current, "opus");
        assert_eq!(converted[2].current, "false");
        assert!(booleans.contains("fast"));
    }

    #[test]
    fn other_agents_keep_their_modes_but_the_elevated_ones() {
        let json = serde_json::json!([
            {"id": "mode", "name": "Mode", "type": "select", "currentValue": "agent",
             "options": [{"value": "read-only", "name": "Read-only"},
                         {"value": "workspace-write", "name": "Workspace access"},
                         {"value": "agent", "name": "Auto review"},
                         {"value": "agent-full-access", "name": "Full access"}]}
        ]);
        let options: Vec<s::SessionConfigOption> = serde_json::from_value(json).unwrap();
        let (converted, _) = convert_options(&options, AcpFlavor::Generic);
        let ids: Vec<&str> = converted[0]
            .values
            .iter()
            .map(|(v, _)| v.as_str())
            .collect();
        assert_eq!(ids, ["read-only", "workspace-write", "agent"]);
        for mode in ["default", "plan", "autoEdit", "read-only", "agent", "build"] {
            assert!(AcpFlavor::Generic.offers_mode(mode), "{mode}");
        }
        for mode in [
            "",
            "yolo",
            "bypassPermissions",
            "agent-full-access",
            "acceptEdits",
        ] {
            assert!(!AcpFlavor::Generic.offers_mode(mode), "{mode}");
        }
        assert!(!AcpFlavor::Claude.offers_mode("agent"));
        // The fallback: `default`, else (not for Claude Code) the first offered mode.
        let modes = |ids: &[&str]| -> Vec<(String, String)> {
            ids.iter()
                .map(|id| ((*id).to_owned(), String::new()))
                .collect()
        };
        assert_eq!(
            fallback_mode(AcpFlavor::Generic, &modes(&["read-only", "agent"])).as_deref(),
            Some("read-only")
        );
        assert_eq!(
            fallback_mode(AcpFlavor::Generic, &modes(&["autoEdit", "default"])).as_deref(),
            Some("default")
        );
        assert_eq!(fallback_mode(AcpFlavor::Claude, &modes(&["plan"])), None);
        assert_eq!(fallback_mode(AcpFlavor::Generic, &[]), None);
    }

    #[test]
    fn prompt_blocks_become_acp_content() {
        let text =
            serde_json::to_value(content_block(PromptBlock::Text("hi".into()), true)).unwrap();
        assert_eq!(text, serde_json::json!({"type": "text", "text": "hi"}));
        let resource = serde_json::to_value(content_block(
            PromptBlock::Resource {
                uri: "file:///p/main.tex".into(),
                mime: "text/x-tex".into(),
                text: "x".into(),
            },
            true,
        ))
        .unwrap();
        assert_eq!(
            resource,
            serde_json::json!({"type": "resource", "resource":
                {"uri": "file:///p/main.tex", "mimeType": "text/x-tex", "text": "x"}})
        );
    }

    #[test]
    fn attached_images_and_documents_become_acp_content() {
        let image = |uri: &str| PromptBlock::Image {
            uri: uri.into(),
            mime: "image/png".into(),
            data: b"PNG".to_vec(),
        };
        let json = |b, images| serde_json::to_value(content_block(b, images)).unwrap();
        assert_eq!(
            json(image("file:///p/fig%201.png"), true),
            serde_json::json!({"type": "image", "data": "UE5H", "mimeType": "image/png",
                               "uri": "file:///p/fig%201.png"})
        );
        assert_eq!(
            json(image(""), true),
            serde_json::json!({"type": "image", "data": "UE5H", "mimeType": "image/png"})
        );
        // An agent without the image capability gets a link to the file instead.
        assert_eq!(
            json(image("file:///p/fig%201.png"), false),
            serde_json::json!({"type": "resource_link", "name": "fig 1.png",
                               "uri": "file:///p/fig%201.png", "mimeType": "image/png",
                               "size": 3})
        );
        assert_eq!(json(image(""), false)["type"], "text");
        let pdf = PromptBlock::Document {
            uri: "file:///p/paper.pdf".into(),
            mime: "application/pdf".into(),
            data: vec![0; 10],
        };
        assert_eq!(
            json(pdf, true),
            serde_json::json!({"type": "resource_link", "name": "paper.pdf",
                               "uri": "file:///p/paper.pdf", "mimeType": "application/pdf",
                               "size": 10})
        );
        // A file too large to include is sent without its bytes: no size.
        let log = PromptBlock::Document {
            uri: "file:///p/huge.log".into(),
            mime: "text/plain".into(),
            data: Vec::new(),
        };
        assert_eq!(
            json(log, true),
            serde_json::json!({"type": "resource_link", "name": "huge.log",
                               "uri": "file:///p/huge.log", "mimeType": "text/plain"})
        );
    }

    #[test]
    fn session_meta_routes_edits_through_lemma() {
        let meta = serde_json::Value::Object(session_meta("APPEND"));
        assert_eq!(
            meta,
            serde_json::json!({
                "claudeCode": {"options": {"disallowedTools": ["Edit", "Write", "NotebookEdit"],
                                           "allowedTools": ["mcp__lemma__*"]}},
                "systemPrompt": {"append": "APPEND"}
            })
        );
    }

    #[test]
    fn incoming_lines_pause_after_a_streak() {
        struct CountWakes(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for CountWakes {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let total = 2 * LINES_PER_POLL + 10;
        let (tx, rx) = async_channel::unbounded();
        for i in 0..total {
            tx.send_blocking(Ok(i.to_string())).unwrap();
        }
        drop(tx);
        let mut lines = CooperativeLines::new(rx);
        let wakes = Arc::new(CountWakes(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut got = Vec::new();
        let mut pauses = Vec::new();
        loop {
            match futures_core::Stream::poll_next(Pin::new(&mut lines), &mut cx) {
                Poll::Ready(Some(line)) => got.push(line.unwrap()),
                Poll::Ready(None) => break,
                Poll::Pending => {
                    // A pause wakes the task at once: the round ends, nothing waits.
                    pauses.push(got.len());
                    assert_eq!(wakes.0.load(Ordering::SeqCst), pauses.len());
                }
            }
        }
        assert_eq!(pauses, [LINES_PER_POLL, 2 * LINES_PER_POLL]);
        let expected: Vec<String> = (0..total).map(|i| i.to_string()).collect();
        assert_eq!(got, expected, "every line, in order");
    }

    #[cfg(unix)]
    #[test]
    fn group_membership_is_read_from_proc() {
        // SAFETY: getpgrp has no preconditions.
        let own = unsafe { libc::getpgrp() };
        assert!(group_has_members(own));
        assert!(!group_has_members(i32::MAX - 7));
    }
}
