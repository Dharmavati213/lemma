//! Finding the agent's toolchain and installing the ACP adapter (WP2.5; ARCHITECTURE §1, §9.7).
//!
//! # Detection ([`detect`])
//!
//! - **node** and **claude**: `agent.node` / `agent.claude` from the config when they name an
//!   executable file; else the first match on `PATH` (mise shims are skipped: they only work
//!   inside an activated shell); else the mise install directories
//!   (`$MISE_DATA_DIR/installs`, by default `~/.local/share/mise/installs`: `node/latest/bin/node`,
//!   `claude/latest/claude`) and, for claude, the native installer's `~/.local/bin/claude`. A
//!   path inside a mise install is stored as mise's `latest` path when that names the same file,
//!   not the canonicalized versioned one, so later updates (`claude update`, a new node) are
//!   picked up.
//! - On the first success (both found and the adapter installed, at least one of them not taken
//!   from the config) they are written to `config.toml` with `lemma_config::set_string`
//!   (comments and order kept). Nothing is written when the config file is not a regular file
//!   (`LEMMA_CONFIG=/dev/null`); a `config.toml` that is a symlink (into a dotfiles repository)
//!   is written where it points, so the link stays.
//! - **The adapter**: `adapter_dir/node_modules/@agentclientprotocol/claude-agent-acp/package.json`
//!   must have version `agent.adapter_version`; otherwise [`ToolchainError::Adapter`], and the
//!   panel offers **Install**.
//! - **claude's version**: read from the canonical path when it is a layout that names claude's
//!   own version (mise's `installs/claude/<x.y.z>/…`, the native installer's
//!   `claude/versions/<x.y.z>`), else from `claude --version` (5 s timeout); an npm-installed
//!   claude sits under node's version directory, which says nothing about claude. Below
//!   [`MIN_CLAUDE_VERSION`] the panel shows a non-blocking "Run `claude update`" hint
//!   ([`needs_update`]).
//!
//! [`detect`] blocks (file system, maybe a subprocess): the bridge runs it with
//! `gio::spawn_blocking`. Its last result is kept for the panel ([`last_result`]), which explains
//! a failed start with it. Order of the checks: node, claude, then the adapter.
//!
//! # Installing ([`install_adapter`])
//!
//! Runs `npm i --prefix <adapter_dir> @agentclientprotocol/claude-agent-acp@<version>
//! --omit=optional --no-audit --no-fund` (npm next to node, else on `PATH`; node's directory
//! first on `PATH` so npm's `#!/usr/bin/env node` finds it), streaming its output line by line,
//! **only when the user clicks Install**. Afterwards the `package.json` version is checked.
//! Tests use a fake `npm` and `claude` on a temporary `PATH`; the real npm is never run.
//!
//! # Windows
//!
//! A program is found by its name plus an extension of `PATHEXT` (`node.exe`, `claude.exe`,
//! npm's `.cmd` shims; [`crate::fsutil::program_file`]). npm is run as `node npm-cli.js` from
//! node's own `node_modules/npm` when it is there, so that no `cmd.exe` stands in between, and
//! through `std::process` in a job object rather than GSubprocess: GLib starts console programs
//! from a GUI program with a console window of their own.
//!
//! # Pi (WP5.2)
//!
//! The `pi` backend runs the pi coding agent through **`@openma/pi-acp`** ([`PI_ADAPTER_PACKAGE`],
//! version `agent.pi_adapter_version`, in `agent.pi_adapter_dir`, default
//! `~/.local/share/lemma/acp-pi`), started as `node <dir>/node_modules/@openma/pi-acp/dist/bin.js`.
//! It was chosen over the other `pi-acp` packages on npm (read with `npm pack`, never run) because
//! it is the one that both routes pi's own `read`/`edit`/`write` tools through the client's ACP
//! `fs/read_text_file`/`fs/write_text_file` when the client advertises them (so Pi's edits become
//! reviewable hunks like Claude's) and registers the `mcpServers` of `session/new` with pi's MCP
//! extension (so Lemma's tools are there too). The upstream `pi-acp` 0.0.34 and its
//! `@gugu-acp`/`@hobby0524` builds do neither (stored, not wired); `@josephjohncox/pi-acp`
//! routes writes through `fs/*` but needs bun and a user systemd daemon. It runs pi in-process on
//! the pi SDK and keeps pi's own settings and credentials in `~/.pi/agent`: logging in is pi's
//! business (`pi`, then `/login`), and Lemma only shows a hint when the adapter answers with ACP's
//! "authentication required". [`detect_pi`] needs node (≥ 22.19 for the adapter) and the
//! adapter; `pi` itself is only looked for, for that hint ([`find_pi`]). [`install_pi_adapter`]
//! runs `npm i --prefix <dir> @openma/pi-acp@<version> --omit=optional --no-audit --no-fund`,
//! only when the user clicks Install.
//!
//! # Other ACP agents ([`detect_agent`])
//!
//! Every backend but Claude Code and the API starts from an [`AgentCommand`]:
//!
//! - **`[agent.<backend>] command`**, when set, is the program (`~` expanded; a bare name is
//!   looked for like the tools below), with exactly `args` and `env`.
//! - **Pi**: node and the Pi adapter, as above.
//! - **Codex**: node and **`@agentclientprotocol/codex-acp`** ([`CODEX_ADAPTER_PACKAGE`], version
//!   `agent.codex_adapter_version`, in `agent.codex_adapter_dir`, default
//!   `~/.local/share/lemma/acp-codex`), started as `node <dir>/node_modules/
//!   @agentclientprotocol/codex-acp/dist/index.js`, with `CODEX_PATH` naming the user's own
//!   `codex` (unless `env` sets it). Like Claude's, the adapter is installed with
//!   `--omit=optional`: the Codex binary it would bring is about 430 MB, and the user's `codex`
//!   (which they log in with, `codex login`) is used instead. Order of the checks: node, codex,
//!   then the adapter ([`ToolchainError::CodexAdapter`]: the panel offers Install,
//!   [`install_codex_adapter`]).
//! - **Agents that speak ACP themselves** ([`preset_command`]): Grok Build (`grok agent stdio`),
//!   Gemini CLI (`gemini --acp`), OpenCode (`opencode acp`) and the GitHub Copilot CLI
//!   (`copilot --acp`). Their tool is looked for on `PATH` (mise shims skipped), in
//!   `~/.local/bin`, `~/.<tool>/bin`, mise's node installs (npm global packages), mise's own
//!   install of the tool and its `npm:` installs ([`find_tool`]). `args`, when set, replace the
//!   preset's arguments.
//! - **Custom**: needs `command` ([`ToolchainError::NoCustomCommand`]).
//!
//! Logging in is the agent's business (`codex login`, `grok login`, …); the panel shows a hint
//! when the agent answers "authentication required". A failure is remembered for
//! [`last_result`], like [`detect_pi`]'s.

use lemma_config::{AgentBackend, AgentCfg, Paths};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::future::Future;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The npm package of the Claude ACP adapter.
pub const ADAPTER_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";
/// The npm package of the Pi ACP adapter (see the module docs).
pub const PI_ADAPTER_PACKAGE: &str = "@openma/pi-acp";
/// The npm package of the Codex ACP adapter (see the module docs).
pub const CODEX_ADAPTER_PACKAGE: &str = "@agentclientprotocol/codex-acp";
/// The oldest `claude` the adapter supports; older ones get the "Run `claude update`" hint.
pub const MIN_CLAUDE_VERSION: (u32, u32, u32) = (2, 1, 293);
/// How long `claude --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Absolute paths of what the agent runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toolchain {
    pub node: PathBuf,
    pub claude: PathBuf,
    /// The adapter's `dist/index.js`.
    pub adapter_js: PathBuf,
    /// `claude --version`, when it could be read.
    pub claude_version: Option<String>,
}

impl Toolchain {
    /// True when [`claude_version`](Self::claude_version) is known and older than
    /// [`MIN_CLAUDE_VERSION`].
    pub fn needs_claude_update(&self) -> bool {
        self.claude_version.as_deref().is_some_and(needs_update)
    }
}

/// Why the agent cannot start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolchainError {
    NodeNotFound,
    ClaudeNotFound,
    /// The adapter is not installed (`found: None`) or has another version than `wanted`; the
    /// panel offers Install.
    Adapter {
        found: Option<String>,
        wanted: String,
    },
    /// The Pi ACP adapter is not installed or has another version (the panel offers Install).
    PiAdapter {
        found: Option<String>,
        wanted: String,
    },
    /// The Codex ACP adapter is not installed or has another version (the panel offers
    /// Install).
    CodexAdapter {
        found: Option<String>,
        wanted: String,
    },
    /// The program `program` that `backend` needs was not found (its command line tool, or the
    /// `command` of its `[agent.<backend>]` table).
    ProgramNotFound {
        backend: AgentBackend,
        program: String,
    },
    /// `backend = "custom"` without `[agent.custom] command`.
    NoCustomCommand,
    /// Anything else, with a message for the user.
    Other(String),
}

impl fmt::Display for ToolchainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodeNotFound => f.write_str("node was not found (set agent.node in config.toml)"),
            Self::ClaudeNotFound => {
                f.write_str("claude was not found (set agent.claude in config.toml)")
            }
            Self::Adapter {
                found: None,
                wanted,
            } => {
                write!(f, "the Claude ACP adapter {wanted} is not installed")
            }
            Self::Adapter {
                found: Some(found),
                wanted,
            } => write!(
                f,
                "the Claude ACP adapter is version {found}, but Lemma needs {wanted}"
            ),
            Self::PiAdapter {
                found: None,
                wanted,
            } => write!(f, "the Pi ACP adapter {wanted} is not installed"),
            Self::PiAdapter {
                found: Some(found),
                wanted,
            } => write!(
                f,
                "the Pi ACP adapter is version {found}, but Lemma needs {wanted}"
            ),
            Self::CodexAdapter {
                found: None,
                wanted,
            } => write!(f, "the Codex ACP adapter {wanted} is not installed"),
            Self::CodexAdapter {
                found: Some(found),
                wanted,
            } => write!(
                f,
                "the Codex ACP adapter is version {found}, but Lemma needs {wanted}"
            ),
            Self::ProgramNotFound { backend, program } => write!(
                f,
                "{program} was not found (set [agent.{}] command in config.toml)",
                backend.as_str()
            ),
            Self::NoCustomCommand => {
                f.write_str("the custom agent has no command (set [agent.custom] command)")
            }
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ToolchainError {}

/// Where [`detect_in`] looks, besides the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Env {
    /// The search path (`PATH`).
    pub path: OsString,
    /// The `config.toml` to store detected paths in; `None` = do not store.
    pub config_file: Option<PathBuf>,
    /// mise's `installs` directory.
    pub mise_installs: PathBuf,
}

impl Env {
    /// The process environment: `PATH`, the settings file of `paths` (`$LEMMA_CONFIG` when set,
    /// see `settings::SettingsFiles::locate`) and `$MISE_DATA_DIR/installs` (default
    /// `~/.local/share/mise/installs`).
    pub fn from_process(paths: &Paths) -> Self {
        let mise = std::env::var_os("MISE_DATA_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| paths.home.join(".local/share/mise"));
        Self {
            path: std::env::var_os("PATH").unwrap_or_default(),
            config_file: crate::settings::SettingsFiles::locate(Some(paths)).map(|f| f.config),
            mise_installs: mise.join("installs"),
        }
    }
}

static LAST: Mutex<Option<Result<Toolchain, ToolchainError>>> = Mutex::new(None);

/// Finds the toolchain in the process environment (see the module docs) and remembers the
/// result for [`last_result`]. Blocking.
pub fn detect(cfg: &AgentCfg, paths: &Paths) -> Result<Toolchain, ToolchainError> {
    let result = detect_in(cfg, paths, &Env::from_process(paths));
    if let Ok(mut last) = LAST.lock() {
        *last = Some(result.clone());
    }
    result
}

/// The result of the last [`detect`] in this process (the bridge's start attempt), if any.
pub fn last_result() -> Option<Result<Toolchain, ToolchainError>> {
    LAST.lock().ok().and_then(|last| last.clone())
}

/// Forgets [`last_result`] (before a new start attempt, and in tests).
pub fn clear_last_result() {
    if let Ok(mut last) = LAST.lock() {
        *last = None;
    }
}

/// [`detect`] with an explicit environment, without remembering the result. Blocking.
pub fn detect_in(cfg: &AgentCfg, paths: &Paths, env: &Env) -> Result<Toolchain, ToolchainError> {
    let node = find_node(cfg, paths, env).ok_or(ToolchainError::NodeNotFound)?;
    let claude = find_claude(cfg, paths, env).ok_or(ToolchainError::ClaudeNotFound)?;
    let adapter_dir = paths.expand(&cfg.adapter_dir);
    let wanted = cfg.adapter_version.clone();
    match adapter_version(&adapter_dir) {
        Some(found) if found == wanted => {}
        found => return Err(ToolchainError::Adapter { found, wanted }),
    }
    // Only a successful detection is remembered.
    if (node.from_config, claude.from_config) != (true, true)
        && let Some(config) = &env.config_file
        && let Err(e) = store_paths(config, &node.path, &claude.path)
    {
        tracing::warn!(
            "could not store the agent paths in {}: {e}",
            config.display()
        );
    }
    let claude_version = claude_version(&claude.path);
    Ok(Toolchain {
        node: node.path,
        claude: claude.path,
        adapter_js: adapter_js(&adapter_dir),
        claude_version,
    })
}

/// What the `pi` backend runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiToolchain {
    pub node: PathBuf,
    /// The adapter's `dist/bin.js`.
    pub adapter_js: PathBuf,
    /// The `pi` command, when found (only for the login hint).
    pub pi: Option<PathBuf>,
}

/// Finds node and the Pi adapter in the process environment (see "Pi" in the module docs). A
/// failure is remembered for [`last_result`] (the panel explains it, and offers Install); a
/// success forgets the last result. Blocking.
pub fn detect_pi(cfg: &AgentCfg, paths: &Paths) -> Result<PiToolchain, ToolchainError> {
    let result = detect_pi_in(cfg, paths, &Env::from_process(paths));
    if let Ok(mut last) = LAST.lock() {
        *last = result.as_ref().err().map(|e| Err(e.clone()));
    }
    result
}

/// [`detect_pi`] with an explicit environment, without remembering the result. Blocking.
pub fn detect_pi_in(
    cfg: &AgentCfg,
    paths: &Paths,
    env: &Env,
) -> Result<PiToolchain, ToolchainError> {
    let node = find_node(cfg, paths, env).ok_or(ToolchainError::NodeNotFound)?;
    let dir = paths.expand(&cfg.pi_adapter_dir);
    let wanted = cfg.pi_adapter_version.clone();
    match package_version(&pi_adapter_package_json(&dir)) {
        Some(found) if found == wanted => {}
        found => return Err(ToolchainError::PiAdapter { found, wanted }),
    }
    Ok(PiToolchain {
        node: node.path,
        adapter_js: pi_adapter_js(&dir),
        pi: find_pi(paths, env),
    })
}

/// The `pi` command: on `PATH` (mise shims skipped), else in mise's node installs (pi is an npm
/// global package: `node/latest/bin/pi`, else the newest `node/<x.y.z>/bin/pi`).
pub fn find_pi(_paths: &Paths, env: &Env) -> Option<PathBuf> {
    search_path(&env.path, "pi", &env.mise_installs)
        .or_else(|| program_file(&env.mise_installs.join("node/latest/bin/pi")))
        .or_else(|| newest_version(&env.mise_installs.join("node"), Path::new("bin/pi")))
}

/// The Pi adapter's entry point under the npm prefix `dir` (already `~`-expanded).
pub fn pi_adapter_js(dir: &Path) -> PathBuf {
    dir.join("node_modules")
        .join(PI_ADAPTER_PACKAGE)
        .join("dist/bin.js")
}

/// The Pi adapter's `package.json` under the npm prefix `dir`.
pub fn pi_adapter_package_json(dir: &Path) -> PathBuf {
    dir.join("node_modules")
        .join(PI_ADAPTER_PACKAGE)
        .join("package.json")
}

/// The installed Pi adapter's version under the npm prefix `dir` (like [`adapter_version`]).
pub fn pi_adapter_version(dir: &Path) -> Option<String> {
    package_version(&pi_adapter_package_json(dir))
}

/// The arguments of the Pi adapter's install command after `npm`.
pub fn pi_install_args(dir: &Path, version: &str) -> Vec<OsString> {
    package_install_args(dir, PI_ADAPTER_PACKAGE, version)
}

/// The whole Pi install command line, for display.
pub fn pi_install_command_line(cfg: &AgentCfg, paths: &Paths) -> String {
    let dir = paths.expand(&cfg.pi_adapter_dir);
    command_line(&pi_install_args(&dir, &cfg.pi_adapter_version))
}

/// Installs the Pi adapter version `cfg.pi_adapter_version` into `cfg.pi_adapter_dir`, like
/// [`install_adapter`] does for Claude's. Run it only when the user asked for it.
pub fn install_pi_adapter<F: Fn(&str) + 'static>(
    cfg: &AgentCfg,
    paths: &Paths,
    env: &Env,
    cancel: &gio::Cancellable,
    on_line: F,
) -> impl Future<Output = Result<(), String>> + use<F> {
    let dir = paths.expand(&cfg.pi_adapter_dir);
    let args = pi_install_args(&dir, &cfg.pi_adapter_version);
    let version = cfg.pi_adapter_version.clone();
    let node = find_node(cfg, paths, env).map(|f| f.path);
    let search = env.path.clone();
    let npm = node.as_deref().and_then(|n| find_npm(n, env));
    let check = move |dir: &Path| match pi_adapter_version(dir) {
        Some(found) if found == version => Ok(()),
        Some(found) => Err(format!(
            "npm finished, but the adapter is version {found} instead of {version}"
        )),
        None => Err("npm finished, but the adapter was not installed".into()),
    };
    run_npm(node, npm, search, dir, args, cancel.clone(), on_line, check)
}

/// How an ACP agent other than Claude Code starts (see "Other ACP agents" in the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    /// The program, absolute.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Environment variables set for the agent.
    pub env: Vec<(OsString, OsString)>,
}

/// The tool and arguments of a backend that speaks ACP itself; `None` for the others.
pub fn preset_command(backend: AgentBackend) -> Option<(&'static str, &'static [&'static str])> {
    match backend {
        AgentBackend::Grok => Some(("grok", &["agent", "stdio"])),
        AgentBackend::Gemini => Some(("gemini", &["--acp"])),
        AgentBackend::OpenCode => Some(("opencode", &["acp"])),
        AgentBackend::Copilot => Some(("copilot", &["--acp"])),
        _ => None,
    }
}

/// Finds how `backend` starts in the process environment (see "Other ACP agents" in the
/// module docs). A failure is remembered for [`last_result`]; a success forgets the last
/// result. Blocking.
pub fn detect_agent(
    cfg: &AgentCfg,
    backend: AgentBackend,
    paths: &Paths,
) -> Result<AgentCommand, ToolchainError> {
    let result = detect_agent_in(cfg, backend, paths, &Env::from_process(paths));
    if let Ok(mut last) = LAST.lock() {
        *last = result.as_ref().err().map(|e| Err(e.clone()));
    }
    result
}

/// [`detect_agent`] with an explicit environment, without remembering the result. Blocking.
pub fn detect_agent_in(
    cfg: &AgentCfg,
    backend: AgentBackend,
    paths: &Paths,
    env: &Env,
) -> Result<AgentCommand, ToolchainError> {
    let table = cfg.acp(backend).ok_or_else(|| {
        ToolchainError::Other(format!("{} is not an ACP agent", backend.as_str()))
    })?;
    let args: Vec<OsString> = table.args.iter().map(OsString::from).collect();
    let mut vars: Vec<(OsString, OsString)> = table
        .env
        .iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    if !table.command.as_os_str().is_empty() {
        let program =
            find_command(&table.command, paths, env).ok_or(ToolchainError::ProgramNotFound {
                backend,
                program: table.command.display().to_string(),
            })?;
        return Ok(AgentCommand {
            program,
            args,
            env: vars,
        });
    }
    let with_script = |script: PathBuf| {
        let mut all = vec![script.into_os_string()];
        all.extend(args.iter().cloned());
        all
    };
    match backend {
        AgentBackend::Pi => {
            let toolchain = detect_pi_in(cfg, paths, env)?;
            Ok(AgentCommand {
                program: toolchain.node,
                args: with_script(toolchain.adapter_js),
                env: vars,
            })
        }
        AgentBackend::Codex => {
            let node = find_node(cfg, paths, env).ok_or(ToolchainError::NodeNotFound)?;
            if !table.env.contains_key("CODEX_PATH") {
                let codex = find_codex(paths, env).ok_or(ToolchainError::ProgramNotFound {
                    backend,
                    program: "codex".into(),
                })?;
                vars.push(("CODEX_PATH".into(), codex.into_os_string()));
            }
            let dir = paths.expand(&cfg.codex_adapter_dir);
            let wanted = cfg.codex_adapter_version.clone();
            match codex_adapter_version(&dir) {
                Some(found) if found == wanted => {}
                found => return Err(ToolchainError::CodexAdapter { found, wanted }),
            }
            Ok(AgentCommand {
                program: node.path,
                args: with_script(codex_adapter_js(&dir)),
                env: vars,
            })
        }
        AgentBackend::Custom => Err(ToolchainError::NoCustomCommand),
        _ => {
            let (tool, preset) = preset_command(backend).ok_or_else(|| {
                ToolchainError::Other(format!("{} has no command", backend.as_str()))
            })?;
            let program = find_tool(tool, paths, env).ok_or(ToolchainError::ProgramNotFound {
                backend,
                program: tool.into(),
            })?;
            Ok(AgentCommand {
                program,
                args: if args.is_empty() {
                    preset.iter().map(OsString::from).collect()
                } else {
                    args
                },
                env: vars,
            })
        }
    }
}

/// The program a configured `command` names: a path (`~` expanded) that must be an absolute
/// executable, or a bare name looked for like [`find_tool`].
pub fn find_command(command: &Path, paths: &Paths, env: &Env) -> Option<PathBuf> {
    let is_name = command.components().count() == 1
        && matches!(command.components().next(), Some(Component::Normal(_)));
    if is_name {
        return find_tool(command.to_str()?, paths, env);
    }
    let path = paths.expand(command);
    if path.is_absolute() {
        program_file(&path)
    } else {
        None
    }
}

/// The command line tool `name` (`grok`, `gemini`, …): on `PATH` (mise shims skipped), else in
/// `~/.local/bin`, `~/.<name>/bin`, mise's node installs (npm global packages: `node/latest`,
/// else the newest version), mise's own install of the tool (`<name>/latest`) and its `npm:`
/// installs (`npm-*/latest/node_modules/.bin`).
pub fn find_tool(name: &str, paths: &Paths, env: &Env) -> Option<PathBuf> {
    let installs = &env.mise_installs;
    search_path(&env.path, name, installs)
        .or_else(|| {
            [
                paths.home.join(".local/bin").join(name),
                paths.home.join(format!(".{name}/bin")).join(name),
                installs.join("node/latest/bin").join(name),
                installs.join(name).join("latest/bin").join(name),
                installs.join(name).join("latest").join(name),
            ]
            .iter()
            .find_map(|p| program_file(p))
        })
        .or_else(|| newest_version(&installs.join("node"), &Path::new("bin").join(name)))
        .or_else(|| {
            let mut npm: Vec<PathBuf> = std::fs::read_dir(installs)
                .ok()?
                .filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().starts_with("npm-"))
                .filter_map(|e| program_file(&e.path().join("latest/node_modules/.bin").join(name)))
                .collect();
            npm.sort();
            npm.into_iter().next()
        })
}

/// The user's `codex`: like [`find_tool`], then the standalone installer's
/// `~/.codex/packages/standalone/current/bin/codex`.
pub fn find_codex(paths: &Paths, env: &Env) -> Option<PathBuf> {
    find_tool("codex", paths, env).or_else(|| {
        program_file(
            &paths
                .home
                .join(".codex/packages/standalone/current/bin/codex"),
        )
    })
}

/// The Codex adapter's entry point under the npm prefix `dir` (already `~`-expanded).
pub fn codex_adapter_js(dir: &Path) -> PathBuf {
    dir.join("node_modules")
        .join(CODEX_ADAPTER_PACKAGE)
        .join("dist/index.js")
}

/// The installed Codex adapter's version under the npm prefix `dir` (like
/// [`adapter_version`]).
pub fn codex_adapter_version(dir: &Path) -> Option<String> {
    package_version(
        &dir.join("node_modules")
            .join(CODEX_ADAPTER_PACKAGE)
            .join("package.json"),
    )
}

/// The arguments of the Codex adapter's install command after `npm`.
pub fn codex_install_args(dir: &Path, version: &str) -> Vec<OsString> {
    package_install_args(dir, CODEX_ADAPTER_PACKAGE, version)
}

/// The whole Codex install command line, for display.
pub fn codex_install_command_line(cfg: &AgentCfg, paths: &Paths) -> String {
    let dir = paths.expand(&cfg.codex_adapter_dir);
    command_line(&codex_install_args(&dir, &cfg.codex_adapter_version))
}

/// Installs the Codex adapter version `cfg.codex_adapter_version` into
/// `cfg.codex_adapter_dir`, like [`install_adapter`] does for Claude's. Run it only when the
/// user asked for it.
pub fn install_codex_adapter<F: Fn(&str) + 'static>(
    cfg: &AgentCfg,
    paths: &Paths,
    env: &Env,
    cancel: &gio::Cancellable,
    on_line: F,
) -> impl Future<Output = Result<(), String>> + use<F> {
    let dir = paths.expand(&cfg.codex_adapter_dir);
    let args = codex_install_args(&dir, &cfg.codex_adapter_version);
    let version = cfg.codex_adapter_version.clone();
    let node = find_node(cfg, paths, env).map(|f| f.path);
    let search = env.path.clone();
    let npm = node.as_deref().and_then(|n| find_npm(n, env));
    let check = move |dir: &Path| match codex_adapter_version(dir) {
        Some(found) if found == version => Ok(()),
        Some(found) => Err(format!(
            "npm finished, but the adapter is version {found} instead of {version}"
        )),
        None => Err("npm finished, but the adapter was not installed".into()),
    };
    run_npm(node, npm, search, dir, args, cancel.clone(), on_line, check)
}

fn package_install_args(dir: &Path, package: &str, version: &str) -> Vec<OsString> {
    vec![
        "i".into(),
        "--prefix".into(),
        dir.as_os_str().to_owned(),
        format!("{package}@{version}").into(),
        "--omit=optional".into(),
        "--no-audit".into(),
        "--no-fund".into(),
    ]
}

fn command_line(args: &[OsString]) -> String {
    let mut line = String::from("npm");
    for arg in args {
        line.push(' ');
        line.push_str(&arg.to_string_lossy());
    }
    line
}

/// A found program and whether it came from the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub from_config: bool,
}

/// Finds `node` (config, `PATH`, mise).
pub fn find_node(cfg: &AgentCfg, paths: &Paths, env: &Env) -> Option<Found> {
    find_program(
        &cfg.node,
        "node",
        paths,
        env,
        &[env.mise_installs.join("node/latest/bin/node")],
        &env.mise_installs.join("node"),
        Path::new("bin/node"),
    )
}

/// Finds `claude` (config, `PATH`, mise, the native installer's `~/.local/bin/claude`).
pub fn find_claude(cfg: &AgentCfg, paths: &Paths, env: &Env) -> Option<Found> {
    find_program(
        &cfg.claude,
        "claude",
        paths,
        env,
        &[
            env.mise_installs.join("claude/latest/claude"),
            env.mise_installs.join("claude/latest/bin/claude"),
            paths.home.join(".local/bin/claude"),
            paths.home.join(".claude/local/claude"),
        ],
        &env.mise_installs.join("claude"),
        Path::new("claude"),
    )
}

/// `npm` for `node`: next to it, else on `PATH`. On Windows npm's own script next to node
/// (`node_modules/npm/bin/npm-cli.js`, which [`run_npm`] runs with node) comes first.
pub fn find_npm(node: &Path, env: &Env) -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(script) = node
        .parent()
        .map(|dir| dir.join(r"node_modules\npm\bin\npm-cli.js"))
        .filter(|p| p.is_file())
    {
        return Some(script);
    }
    node.parent()
        .and_then(|dir| program_file(&dir.join("npm")))
        .or_else(|| search_path(&env.path, "npm", &env.mise_installs))
}

fn find_program(
    configured: &Path,
    name: &str,
    paths: &Paths,
    env: &Env,
    fixed: &[PathBuf],
    versions_dir: &Path,
    in_version: &Path,
) -> Option<Found> {
    if !configured.as_os_str().is_empty() {
        let path = paths.expand(configured);
        if path.is_absolute()
            && let Some(path) = program_file(&path)
        {
            return Some(Found {
                path,
                from_config: true,
            });
        }
        tracing::warn!(
            "agent: the configured {name} {} is not an executable file; looking for another",
            path.display()
        );
    }
    let found = search_path(&env.path, name, &env.mise_installs)
        .or_else(|| fixed.iter().find_map(|p| program_file(p)))
        .or_else(|| newest_version(versions_dir, in_version))?;
    Some(Found {
        path: prefer_mise_latest(&found, &env.mise_installs),
        from_config: false,
    })
}

/// The first executable `name` on `search` (a `PATH` value), skipping mise's shims and relative
/// entries.
fn search_path(search: &OsStr, name: &str, mise_installs: &Path) -> Option<PathBuf> {
    let shims = mise_installs.parent().map(|m| m.join("shims"));
    std::env::split_paths(search)
        .filter(|dir| dir.is_absolute())
        .filter(|dir| shims.as_deref() != Some(dir.as_path()))
        .find_map(|dir| program_file(&dir.join(name)))
}

/// `dir/<highest x.y.z>/in_version` when it is executable.
fn newest_version(dir: &Path, in_version: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            Some((
                parse_version(&name)?,
                program_file(&e.path().join(in_version))?,
            ))
        })
        .max_by_key(|(v, _)| *v)
        .map(|(_, p)| p)
}

/// `<installs>/<tool>/latest/<rest>` for `<installs>/<tool>/<version>/<rest>` when both name the
/// same file; else `path`.
fn prefer_mise_latest(path: &Path, installs: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix(installs) else {
        return path.to_path_buf();
    };
    let mut parts = rest.components();
    let (Some(Component::Normal(tool)), Some(Component::Normal(version))) =
        (parts.next(), parts.next())
    else {
        return path.to_path_buf();
    };
    if version == "latest" {
        return path.to_path_buf();
    }
    let latest = installs.join(tool).join("latest").join(parts.as_path());
    match (
        lemma_core::canonicalize(&latest),
        lemma_core::canonicalize(path),
    ) {
        (Ok(a), Ok(b)) if a == b => latest,
        _ => path.to_path_buf(),
    }
}

/// The program `path` names (see [`crate::fsutil::program_file`]).
fn program_file(path: &Path) -> Option<PathBuf> {
    crate::fsutil::program_file(path)
}

/// Writes `agent.node` and `agent.claude` into the config file (created if missing), keeping the
/// rest of its text. Nothing happens when the file exists but is not a regular file. A config
/// that is a symlink (into a dotfiles repository) is written where it points, with the target's
/// permissions, so the link stays.
fn store_paths(config: &Path, node: &Path, claude: &Path) -> Result<(), String> {
    let target = match std::fs::symlink_metadata(config) {
        Ok(m) if m.file_type().is_symlink() => lemma_core::canonicalize(config)
            .map_err(|e| format!("the config file is a symlink that cannot be followed: {e}"))?,
        _ => config.to_path_buf(),
    };
    let config = target.as_path();
    let (text, permissions) = match std::fs::metadata(config) {
        Ok(m) if !m.is_file() => return Ok(()),
        Ok(m) => (
            std::fs::read_to_string(config).map_err(|e| e.to_string())?,
            Some(m.permissions()),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(e.to_string()),
    };
    let utf8 = |p: &Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{} is not UTF-8", p.display()))
    };
    let text = lemma_config::set_string(&text, "agent", "node", &utf8(node)?)?;
    let text = lemma_config::set_string(&text, "agent", "claude", &utf8(claude)?)?;
    let dir = config.parent().ok_or("the config file has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".config.toml.lemma-{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    if let Some(permissions) = permissions
        && let Err(e) = std::fs::set_permissions(&tmp, permissions)
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    std::fs::rename(&tmp, config).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

/// The installed adapter's version under the npm prefix `adapter_dir` (`None` when it is not
/// installed; `Some("unreadable")` when its `package.json` cannot be read).
pub fn adapter_version(adapter_dir: &Path) -> Option<String> {
    package_version(&adapter_package_json(adapter_dir))
}

/// The `version` of the `package.json` at `path` (`None` when it does not exist,
/// `Some("unreadable")` when it cannot be read).
fn package_version(path: &Path) -> Option<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some("unreadable".into()),
    };
    let version = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("version")?.as_str().map(str::to_owned));
    Some(version.unwrap_or_else(|| "unreadable".into()))
}

/// The version of the `claude` at `path`: from its canonical path when that is one of the
/// layouts that name claude's own version (mise's `…/installs/claude/<x.y.z>/…` and the native
/// installer's `…/claude/versions/<x.y.z>`), else from `claude --version`. Any other `x.y.z`
/// in the path is not claude's (an npm-installed claude lives under node's version directory).
pub fn claude_version(path: &Path) -> Option<String> {
    if let Some(version) = lemma_core::canonicalize(path)
        .ok()
        .and_then(|canonical| version_in_claude_layout(&canonical))
    {
        return Some(version);
    }
    let output = run_with_timeout(path, &["--version"], VERSION_TIMEOUT)?;
    output
        .split_whitespace()
        .find(|word| parse_version(word).is_some())
        .map(str::to_owned)
}

/// The deepest `x.y.z` component of `path` that sits right under a `claude` (or `claude-code`)
/// directory, or under its `versions` directory.
fn version_in_claude_layout(path: &Path) -> Option<String> {
    let names: Vec<&str> = path
        .components()
        .map(|c| match c {
            Component::Normal(name) => name.to_str().unwrap_or(""),
            _ => "",
        })
        .collect();
    let is_claude = |name: &str| matches!(name, "claude" | "claude-code");
    (1..names.len()).rev().find_map(|i| {
        let parent = names[i - 1];
        let ours = is_claude(parent) || (parent == "versions" && i >= 2 && is_claude(names[i - 2]));
        (ours && parse_version(names[i]).is_some()).then(|| names[i].to_owned())
    })
}

/// Runs `program args`, returning its stdout when it exits with success within `timeout`.
fn run_with_timeout(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = lemma_core::process::command(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    status.success().then_some(out)
}

/// `x.y.z` (each part digits only) as numbers.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.');
    let mut next = || -> Option<u32> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// True when `version` is a parsable version older than [`MIN_CLAUDE_VERSION`].
pub fn needs_update(version: &str) -> bool {
    parse_version(version).is_some_and(|v| v < MIN_CLAUDE_VERSION)
}

/// The adapter's entry point under the npm prefix `adapter_dir` (already `~`-expanded).
pub fn adapter_js(adapter_dir: &Path) -> PathBuf {
    adapter_package_dir(adapter_dir).join("dist/index.js")
}

/// The adapter's `package.json` under the npm prefix `adapter_dir`.
pub fn adapter_package_json(adapter_dir: &Path) -> PathBuf {
    adapter_package_dir(adapter_dir).join("package.json")
}

fn adapter_package_dir(adapter_dir: &Path) -> PathBuf {
    adapter_dir.join("node_modules").join(ADAPTER_PACKAGE)
}

/// The arguments of the install command after `npm` (see the module docs).
pub fn install_args(adapter_dir: &Path, version: &str) -> Vec<OsString> {
    package_install_args(adapter_dir, ADAPTER_PACKAGE, version)
}

/// The whole install command line, for display (`npm i --prefix … --no-fund`).
pub fn install_command_line(cfg: &AgentCfg, paths: &Paths) -> String {
    let dir = paths.expand(&cfg.adapter_dir);
    command_line(&install_args(&dir, &cfg.adapter_version))
}

/// Installs the adapter version `cfg.adapter_version` into `cfg.adapter_dir` with npm (see the
/// module docs), calling `on_line` with every line of npm's output (stdout and stderr). Resolves
/// when npm has finished and the installed version is the wanted one. Cancelling `cancel` kills
/// npm. Run it only when the user asked for it.
pub fn install_adapter<F: Fn(&str) + 'static>(
    cfg: &AgentCfg,
    paths: &Paths,
    env: &Env,
    cancel: &gio::Cancellable,
    on_line: F,
) -> impl Future<Output = Result<(), String>> + use<F> {
    let adapter_dir = paths.expand(&cfg.adapter_dir);
    let version = cfg.adapter_version.clone();
    let args = install_args(&adapter_dir, &version);
    let node = find_node(cfg, paths, env).map(|f| f.path);
    let npm = node.as_deref().and_then(|n| find_npm(n, env));
    let search = env.path.clone();
    let check = move |dir: &Path| match adapter_version(dir) {
        Some(found) if found == version => Ok(()),
        Some(found) => Err(format!(
            "npm finished, but the adapter is version {found} instead of {version}"
        )),
        None => Err("npm finished, but the adapter was not installed".into()),
    };
    run_npm(
        node,
        npm,
        search,
        adapter_dir,
        args,
        cancel.clone(),
        on_line,
        check,
    )
}

/// Runs `npm args` in `adapter_dir` (see the module docs), then `check`s the result.
#[allow(clippy::too_many_arguments)]
async fn run_npm<F, C>(
    node: Option<PathBuf>,
    npm: Option<PathBuf>,
    search: OsString,
    adapter_dir: PathBuf,
    args: Vec<OsString>,
    cancel: gio::Cancellable,
    on_line: F,
    check: C,
) -> Result<(), String>
where
    F: Fn(&str) + 'static,
    C: FnOnce(&Path) -> Result<(), String> + 'static,
{
    let node = node.ok_or("node was not found, so npm cannot run")?;
    let npm = npm.ok_or_else(|| format!("npm was not found next to {}", node.display()))?;
    std::fs::create_dir_all(&adapter_dir)
        .map_err(|e| format!("cannot create {}: {e}", adapter_dir.display()))?;
    // npm's `#!/usr/bin/env node` must find this node.
    let mut dirs: Vec<PathBuf> = node.parent().map(Path::to_path_buf).into_iter().collect();
    dirs.extend(std::env::split_paths(&search));
    let joined = std::env::join_paths(dirs).map_err(|e| e.to_string())?;
    // npm's own script runs with node (Windows, see `find_npm`).
    let mut argv: Vec<OsString> = if npm.extension().is_some_and(|e| e == "js") {
        vec![node.into_os_string(), npm.into_os_string()]
    } else {
        vec![npm.into_os_string()]
    };
    argv.extend(args);
    #[cfg(windows)]
    run_streaming(argv, joined, &adapter_dir, &cancel, &on_line).await?;
    #[cfg(unix)]
    run_subprocess(argv, joined, &adapter_dir, &cancel, &on_line).await?;
    check(&adapter_dir)
}

/// Runs `argv` in `cwd` with `PATH` set to `path`, calling `on_line` with every line of its
/// output (stdout and stderr), until it exits; cancelling `cancel` kills it.
#[cfg(unix)]
async fn run_subprocess(
    argv: Vec<OsString>,
    path: OsString,
    cwd: &Path,
    cancel: &gio::Cancellable,
    on_line: &impl Fn(&str),
) -> Result<(), String> {
    use gio::prelude::*;
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_PIPE
            | gio::SubprocessFlags::STDERR_MERGE
            | gio::SubprocessFlags::STDIN_INHERIT,
    );
    launcher.setenv("PATH", &path, true);
    launcher.setenv("NO_UPDATE_NOTIFIER", "1", true);
    launcher.setenv("npm_config_update_notifier", "false", true);
    launcher.set_cwd(cwd);
    let argv: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
    let process = launcher
        .spawn(&argv)
        .map_err(|e| format!("cannot run npm: {e}"))?;
    let killer = process.clone();
    let handler = cancel.connect_cancelled_local(move |_| killer.force_exit());
    let stdout = process.stdout_pipe().ok_or("npm has no output pipe")?;
    let lines = gio::DataInputStream::new(&stdout);
    loop {
        match lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
            Ok(Some(line)) => on_line(line.trim_end_matches('\r')),
            Ok(None) => break,
            Err(e) => {
                on_line(&format!("(could not read npm's output: {e})"));
                break;
            }
        }
    }
    let waited = process.wait_future().await;
    if let Some(handler) = handler {
        cancel.disconnect_cancelled(handler);
    }
    if cancel.is_cancelled() {
        return Err("the installation was cancelled".into());
    }
    waited.map_err(|e| format!("npm failed: {e}"))?;
    if !process.has_exited() || process.exit_status() != 0 {
        return Err(format!(
            "npm failed (exit status {})",
            if process.has_exited() {
                process.exit_status().to_string()
            } else {
                "unknown".into()
            }
        ));
    }
    Ok(())
}

/// [`run_subprocess`] for Windows: `std::process` in a job object, its output read on a thread
/// (see "Windows" in the module docs). stdin is empty.
#[cfg(windows)]
async fn run_streaming(
    argv: Vec<OsString>,
    path: OsString,
    cwd: &Path,
    cancel: &gio::Cancellable,
    on_line: &impl Fn(&str),
) -> Result<(), String> {
    use gio::prelude::*;
    use std::io::BufRead as _;
    let cannot_run = |e: std::io::Error| format!("cannot run npm: {e}");
    let (reader, writer) = std::io::pipe().map_err(cannot_run)?;
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env("PATH", &path)
        .env("NO_UPDATE_NOTIFIER", "1")
        .env("npm_config_update_notifier", "false")
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(writer.try_clone().map_err(cannot_run)?)
        .stderr(writer);
    let (mut child, job) = lemma_core::process::Job::spawn(&mut command).map_err(cannot_run)?;
    // The command holds the pipe's write ends: without them, the reader sees the end of the
    // output once npm (and whatever it started) is done.
    drop(command);
    let job = std::sync::Arc::new(job);
    let killer = job.clone();
    let handler = cancel.connect_cancelled_local(move |_| killer.terminate(1));
    let (lines_tx, lines) = async_channel::unbounded::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(reader).split(b'\n') {
            let Ok(line) = line else { break };
            if lines_tx
                .send_blocking(String::from_utf8_lossy(&line).into_owned())
                .is_err()
            {
                break;
            }
        }
    });
    while let Ok(line) = lines.recv().await {
        on_line(line.trim_end_matches('\r'));
    }
    let status = gio::spawn_blocking(move || child.wait()).await;
    if let Some(handler) = handler {
        cancel.disconnect_cancelled(handler);
    }
    drop(job);
    if cancel.is_cancelled() {
        return Err("the installation was cancelled".into());
    }
    match status {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!(
            "npm failed (exit status {})",
            status
                .code()
                .map_or_else(|| "unknown".into(), |c| c.to_string())
        )),
        Ok(Err(e)) => Err(format!("npm failed: {e}")),
        Err(_) => Err("npm failed: its waiter panicked".into()),
    }
}

// The tests run fake `node`, `npm` and `claude` shell scripts.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn adapter_paths() {
        let dir = Path::new("/home/u/.local/share/lemma/acp");
        assert_eq!(
            adapter_js(dir),
            PathBuf::from(
                "/home/u/.local/share/lemma/acp/node_modules/@agentclientprotocol/\
                 claude-agent-acp/dist/index.js"
            )
        );
        assert_eq!(
            adapter_package_json(dir),
            PathBuf::from(
                "/home/u/.local/share/lemma/acp/node_modules/@agentclientprotocol/\
                 claude-agent-acp/package.json"
            )
        );
        assert_eq!(
            install_args(dir, "0.89.1"),
            [
                "i",
                "--prefix",
                "/home/u/.local/share/lemma/acp",
                "@agentclientprotocol/claude-agent-acp@0.89.1",
                "--omit=optional",
                "--no-audit",
                "--no-fund"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("2.1.293"), Some((2, 1, 293)));
        assert_eq!(parse_version("2.1"), None);
        assert_eq!(parse_version("2.1.293.4"), None);
        assert_eq!(parse_version("v2.1.293"), None);
        assert_eq!(parse_version("2.1.x"), None);
        assert!(needs_update("2.1.288"));
        assert!(needs_update("1.9.999"));
        assert!(!needs_update("2.1.293"));
        assert!(!needs_update("2.2.0"));
        assert!(!needs_update("garbage"));
    }

    /// A temporary home with a `bin` directory for fake programs.
    struct Home {
        dir: PathBuf,
    }

    impl Home {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("lemma-install-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            Self { dir }
        }

        fn paths(&self) -> Paths {
            let home = self.dir.clone();
            Paths::from_vars(move |k| (k == "HOME").then(|| home.clone().into_os_string())).unwrap()
        }

        fn env(&self) -> Env {
            Env {
                path: std::env::join_paths([
                    self.dir.join("bin"),
                    "/usr/bin".into(),
                    "/bin".into(),
                ])
                .unwrap(),
                config_file: Some(self.dir.join("config/lemma/config.toml")),
                mise_installs: self.dir.join(".local/share/mise/installs"),
            }
        }

        fn script(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn adapter(&self, version: &str) {
            let pkg = adapter_package_json(&self.dir.join(".local/share/lemma/acp"));
            std::fs::create_dir_all(pkg.parent().unwrap()).unwrap();
            std::fs::write(pkg, format!("{{\"name\":\"x\",\"version\":\"{version}\"}}")).unwrap();
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn detection_order_and_errors() {
        let home = Home::new("order");
        let (paths, env) = (home.paths(), home.env());
        let mut cfg = AgentCfg::default();
        assert_eq!(
            detect_in(&cfg, &paths, &env),
            Err(ToolchainError::NodeNotFound)
        );
        let node = home.script("bin/node", "exit 0");
        assert_eq!(
            detect_in(&cfg, &paths, &env),
            Err(ToolchainError::ClaudeNotFound)
        );
        let claude = home.script("bin/claude", "echo '2.1.296 (Claude Code)'");
        assert_eq!(
            detect_in(&cfg, &paths, &env),
            Err(ToolchainError::Adapter {
                found: None,
                wanted: "0.89.1".into()
            })
        );
        home.adapter("0.88.0");
        assert_eq!(
            detect_in(&cfg, &paths, &env),
            Err(ToolchainError::Adapter {
                found: Some("0.88.0".into()),
                wanted: "0.89.1".into()
            })
        );
        home.adapter("0.89.1");
        let toolchain = detect_in(&cfg, &paths, &env).unwrap();
        assert_eq!(toolchain.node, node);
        assert_eq!(toolchain.claude, claude);
        assert_eq!(toolchain.claude_version.as_deref(), Some("2.1.296"));
        assert!(!toolchain.needs_claude_update());
        assert_eq!(
            toolchain.adapter_js,
            adapter_js(&home.dir.join(".local/share/lemma/acp"))
        );

        // The paths were stored in the config on this first success …
        let config = std::fs::read_to_string(env.config_file.as_ref().unwrap()).unwrap();
        let parsed = lemma_config::parse(&config).unwrap();
        assert_eq!(parsed.agent.node, node);
        assert_eq!(parsed.agent.claude, claude);
        // … and the config wins from then on, also over PATH.
        cfg = parsed.agent;
        let other = home.script("elsewhere/node", "exit 0");
        cfg.node = other.clone();
        assert_eq!(detect_in(&cfg, &paths, &env).unwrap().node, other);
        // A configured path that went away falls back to the search (and is stored again).
        cfg.node = home.dir.join("gone/node");
        assert_eq!(detect_in(&cfg, &paths, &env).unwrap().node, node);
    }

    #[test]
    fn old_claude_gets_the_update_hint_and_a_hanging_one_none() {
        let home = Home::new("old");
        let (paths, env) = (home.paths(), home.env());
        home.script("bin/node", "exit 0");
        home.adapter("0.89.1");
        home.script("bin/claude", "echo 2.1.288 '(Claude Code)'");
        let toolchain = detect_in(&AgentCfg::default(), &paths, &env).unwrap();
        assert_eq!(toolchain.claude_version.as_deref(), Some("2.1.288"));
        assert!(toolchain.needs_claude_update());
        // `--version` failing or printing nothing: unknown, no hint.
        home.script("bin/claude", "exit 3");
        let toolchain = detect_in(&AgentCfg::default(), &paths, &env).unwrap();
        assert_eq!(toolchain.claude_version, None);
        assert!(!toolchain.needs_claude_update());
    }

    #[test]
    fn mise_installs_prefer_latest_and_skip_shims() {
        let home = Home::new("mise");
        let paths = home.paths();
        let mut env = home.env();
        let installs = env.mise_installs.clone();
        // Shims on PATH are skipped.
        home.script(".local/share/mise/shims/node", "exit 0");
        home.script(".local/share/mise/shims/claude", "exit 0");
        env.path = std::env::join_paths([
            home.dir.join(".local/share/mise/shims"),
            home.dir.join(".local/share/mise/installs/node/26.7.0/bin"),
            "/usr/bin".into(),
        ])
        .unwrap();
        let versioned = home.script(".local/share/mise/installs/node/26.7.0/bin/node", "exit 0");
        std::os::unix::fs::symlink("./26.7.0", installs.join("node/latest")).unwrap();
        let found = find_node(&AgentCfg::default(), &paths, &env).unwrap();
        assert_eq!(found.path, installs.join("node/latest/bin/node"));
        assert!(!found.from_config);
        assert_ne!(found.path, versioned);
        // claude is not on PATH: mise's latest, whose version comes from the directory name.
        home.script(".local/share/mise/installs/claude/2.1.200/claude", "exit 9");
        home.script(".local/share/mise/installs/claude/2.1.296/claude", "exit 9");
        std::os::unix::fs::symlink("./2.1.296", installs.join("claude/latest")).unwrap();
        let claude = find_claude(&AgentCfg::default(), &paths, &env).unwrap();
        assert_eq!(claude.path, installs.join("claude/latest/claude"));
        assert_eq!(claude_version(&claude.path).as_deref(), Some("2.1.296"));
        // Without a `latest` link, the newest version directory.
        std::fs::remove_file(installs.join("claude/latest")).unwrap();
        let claude = find_claude(&AgentCfg::default(), &paths, &env).unwrap();
        assert_eq!(claude.path, installs.join("claude/2.1.296/claude"));
        // npm next to node.
        let npm = home.script(".local/share/mise/installs/node/26.7.0/bin/npm", "exit 0");
        assert_eq!(
            find_npm(&found.path, &env).map(|p| p.canonicalize().unwrap()),
            Some(npm.canonicalize().unwrap())
        );
    }

    #[test]
    fn storing_keeps_the_config_text() {
        let home = Home::new("store");
        let config = home.dir.join("config.toml");
        std::fs::write(
            &config,
            "# mine\n[editor]\nvim = false # keep\n\n[agent]\nmodel = \"opus\"\n",
        )
        .unwrap();
        store_paths(&config, Path::new("/n/node"), Path::new("/c/claude")).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(
            text.starts_with("# mine\n[editor]\nvim = false # keep\n"),
            "{text}"
        );
        let parsed = lemma_config::parse(&text).unwrap();
        assert_eq!(parsed.agent.node, Path::new("/n/node"));
        assert_eq!(parsed.agent.claude, Path::new("/c/claude"));
        assert_eq!(parsed.agent.model, "opus");
        assert!(!parsed.editor.vim);
        // Not a regular file: nothing is written.
        assert_eq!(
            store_paths(Path::new("/dev/null"), Path::new("/n"), Path::new("/c")),
            Ok(())
        );
    }

    #[test]
    fn a_symlinked_config_is_written_where_it_points() {
        let home = Home::new("symlink");
        let (paths, mut env) = (home.paths(), home.env());
        home.script("bin/node", "exit 0");
        home.script("bin/claude", "echo '2.1.296 (Claude Code)'");
        home.adapter("0.89.1");
        let dotfiles = home.dir.join("dotfiles/lemma/config.toml");
        std::fs::create_dir_all(dotfiles.parent().unwrap()).unwrap();
        std::fs::write(&dotfiles, "# mine\n[editor]\nvim = true\n").unwrap();
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = home.dir.join("config/lemma/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&dotfiles, &config).unwrap();
        env.config_file = Some(config.clone());
        detect_in(&AgentCfg::default(), &paths, &env).unwrap();
        assert!(std::fs::symlink_metadata(&config).unwrap().is_symlink());
        let parsed = lemma_config::parse(&std::fs::read_to_string(&dotfiles).unwrap()).unwrap();
        assert_eq!(parsed.agent.node, home.dir.join("bin/node"));
        assert!(parsed.editor.vim);
        let mode = std::fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // No temporary file is left in either directory.
        for dir in [dotfiles.parent().unwrap(), config.parent().unwrap()] {
            let names: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
            assert_eq!(names.len(), 1, "{names:?}");
        }
    }

    #[test]
    fn claude_version_only_from_claude_layouts() {
        let path = |s: &str| PathBuf::from(s);
        assert_eq!(
            version_in_claude_layout(&path("/h/.local/share/mise/installs/claude/2.1.296/claude"))
                .as_deref(),
            Some("2.1.296")
        );
        assert_eq!(
            version_in_claude_layout(&path("/h/.local/share/claude/versions/2.1.300")).as_deref(),
            Some("2.1.300")
        );
        // node's version directory says nothing about claude.
        assert_eq!(
            version_in_claude_layout(&path(
                "/h/mise/installs/node/22.11.0/lib/node_modules/@anthropic-ai/claude-code/cli.js"
            )),
            None
        );
        assert_eq!(
            version_in_claude_layout(&path("/opt/1.2.3/bin/claude")),
            None
        );
    }

    /// `home`'s environment with only its `bin` on `PATH` (no system node, grok or codex).
    fn private_env(home: &Home) -> Env {
        Env {
            path: home.dir.join("bin").into_os_string(),
            ..home.env()
        }
    }

    fn os(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn agents_that_speak_acp_start_with_their_presets() {
        let home = Home::new("acp-presets");
        let (paths, env) = (home.paths(), private_env(&home));
        let mut cfg = AgentCfg::default();
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Grok, &paths, &env),
            Err(ToolchainError::ProgramNotFound {
                backend: AgentBackend::Grok,
                program: "grok".into()
            })
        );
        // mise's `npm:` install, when it is not on PATH.
        let npm = home.script(
            ".local/share/mise/installs/npm-xai-official-grok/latest/node_modules/.bin/grok",
            "exit 0",
        );
        let command = detect_agent_in(&cfg, AgentBackend::Grok, &paths, &env).unwrap();
        assert_eq!(command.program, npm);
        assert_eq!(command.args, os(&["agent", "stdio"]));
        assert!(command.env.is_empty());
        // `~/.local/bin` comes first, PATH before that.
        let local = home.script(".local/bin/grok", "exit 0");
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Grok, &paths, &env)
                .unwrap()
                .program,
            local
        );
        let on_path = home.script("bin/grok", "exit 0");
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Grok, &paths, &env)
                .unwrap()
                .program,
            on_path
        );
        // `args` replace the preset's; `env` is passed on.
        cfg.grok.args = vec!["agent".into(), "stdio".into(), "--debug".into()];
        cfg.grok.env.insert("GROK_SANDBOX".into(), "strict".into());
        let command = detect_agent_in(&cfg, AgentBackend::Grok, &paths, &env).unwrap();
        assert_eq!(command.args, os(&["agent", "stdio", "--debug"]));
        assert_eq!(command.env, [("GROK_SANDBOX".into(), "strict".into())]);
        // The other presets.
        for (backend, tool, args) in [
            (AgentBackend::Gemini, "gemini", &["--acp"][..]),
            (AgentBackend::OpenCode, "opencode", &["acp"][..]),
            (AgentBackend::Copilot, "copilot", &["--acp"][..]),
        ] {
            let program = home.script(&format!(".{tool}/bin/{tool}"), "exit 0");
            let command = detect_agent_in(&cfg, backend, &paths, &env).unwrap();
            assert_eq!((command.program, command.args), (program, os(args)));
        }
        // Claude Code and the API have no such command.
        assert!(matches!(
            detect_agent_in(&cfg, AgentBackend::Claude, &paths, &env),
            Err(ToolchainError::Other(_))
        ));
    }

    #[test]
    fn a_configured_command_wins() {
        let home = Home::new("acp-command");
        let (paths, env) = (home.paths(), private_env(&home));
        let mut cfg = AgentCfg::default();
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Custom, &paths, &env),
            Err(ToolchainError::NoCustomCommand)
        );
        cfg.custom.command = "~/agents/goose".into();
        cfg.custom.args = vec!["acp".into()];
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Custom, &paths, &env),
            Err(ToolchainError::ProgramNotFound {
                backend: AgentBackend::Custom,
                program: "~/agents/goose".into()
            })
        );
        let goose = home.script("agents/goose", "exit 0");
        let command = detect_agent_in(&cfg, AgentBackend::Custom, &paths, &env).unwrap();
        assert_eq!(command.program, goose);
        assert_eq!(command.args, os(&["acp"]));
        // A bare name is looked for like the presets' tools.
        cfg.custom.command = "kimi".into();
        let kimi = home.script("bin/kimi", "exit 0");
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Custom, &paths, &env)
                .unwrap()
                .program,
            kimi
        );
        // A relative path is not a program.
        cfg.custom.command = "bin/kimi".into();
        assert!(detect_agent_in(&cfg, AgentBackend::Custom, &paths, &env).is_err());
        // For Pi and Codex it replaces node and the adapter: nothing else is checked.
        cfg.codex.command = goose.clone();
        let command = detect_agent_in(&cfg, AgentBackend::Codex, &paths, &env).unwrap();
        assert_eq!((command.program, command.args), (goose, Vec::new()));
    }

    #[test]
    fn codex_needs_node_its_cli_and_the_adapter() {
        let home = Home::new("acp-codex");
        let (paths, env) = (home.paths(), private_env(&home));
        let mut cfg = AgentCfg::default();
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Codex, &paths, &env),
            Err(ToolchainError::NodeNotFound)
        );
        let node = home.script("bin/node", "exit 0");
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Codex, &paths, &env),
            Err(ToolchainError::ProgramNotFound {
                backend: AgentBackend::Codex,
                program: "codex".into()
            })
        );
        // The standalone installer's codex.
        let codex = home.script(".codex/packages/standalone/current/bin/codex", "exit 0");
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Codex, &paths, &env),
            Err(ToolchainError::CodexAdapter {
                found: None,
                wanted: "2.2.2".into()
            })
        );
        let dir = home.dir.join(".local/share/lemma/acp-codex");
        let pkg = dir.join("node_modules/@agentclientprotocol/codex-acp/package.json");
        std::fs::create_dir_all(pkg.parent().unwrap()).unwrap();
        std::fs::write(&pkg, r#"{"version":"2.2.1"}"#).unwrap();
        assert_eq!(
            detect_agent_in(&cfg, AgentBackend::Codex, &paths, &env),
            Err(ToolchainError::CodexAdapter {
                found: Some("2.2.1".into()),
                wanted: "2.2.2".into()
            })
        );
        std::fs::write(&pkg, r#"{"version":"2.2.2"}"#).unwrap();
        let command = detect_agent_in(&cfg, AgentBackend::Codex, &paths, &env).unwrap();
        assert_eq!(command.program, node);
        assert_eq!(command.args, [codex_adapter_js(&dir).into_os_string()]);
        assert_eq!(
            command.env,
            [("CODEX_PATH".into(), codex.clone().into_os_string())]
        );
        // `CODEX_PATH` in the table wins (and then no codex need be found).
        std::fs::remove_file(&codex).unwrap();
        cfg.codex
            .env
            .insert("CODEX_PATH".into(), "/opt/codex/bin/codex".into());
        let command = detect_agent_in(&cfg, AgentBackend::Codex, &paths, &env).unwrap();
        assert_eq!(
            command.env,
            [("CODEX_PATH".into(), "/opt/codex/bin/codex".into())]
        );
        assert_eq!(
            codex_install_args(&dir, "2.2.2"),
            os(&[
                "i",
                "--prefix",
                dir.to_str().unwrap(),
                "@agentclientprotocol/codex-acp@2.2.2",
                "--omit=optional",
                "--no-audit",
                "--no-fund"
            ])
        );
    }
}
