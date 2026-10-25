# Testing Lemma

This page covers how to run every check, how GUI tests stay off the desktop, and how the fixtures,
performance tests, golden images and memory budgets work. The conventions come from
`docs/ARCHITECTURE.md` §10.1.

## Quick reference

| What | Command |
|---|---|
| Everything CI checks | `scripts/ci.sh` |
| One crate | `cargo fmt -p <crate>`, `cargo clippy -p <crate> --all-targets -- -D warnings`, `cargo test -p <crate>` |
| A GUI test or the app | `scripts/broadway.sh cargo test -p lemma-app` or `scripts/broadway.sh <binary> …` |
| Performance tests | `scripts/broadway.sh cargo test --release --workspace -- --ignored perf --test-threads=1` |
| The agent end to end (fake agent, real binary) | `scripts/e2e-agent.sh [case…]` |
| Every documentation screenshot | `scripts/shots.sh [name…]` |
| Large fixtures (50-page PDF, 1k-line document) | `scripts/build-fixtures.sh` |
| Screenshot | `scripts/shot.sh out.png tests/fixtures/proj/main.tex` |
| Memory budgets | `scripts/rss.sh [idle\|pdf50\|large]` |
| Timing gate on the real Wayland desktop (by hand; opens windows) | `scripts/perf-wayland.sh [startup] [typing] [pdf]` |

All scripts respect `CARGO_TARGET_DIR`.

## `scripts/ci.sh`

Runs, in order, and stops at the first failure:

1. `bash -n` on every `scripts/*.sh`;
2. `cargo fmt --all --check`;
3. `cargo run -p xtask -- gen-completion --check` (the generated completion tables are current);
4. `cargo clippy --workspace --all-targets -- -D warnings`;
5. `cargo test --workspace` under `scripts/broadway.sh`, so the GTK tests run offscreen.

The tests get a **private Broadway display** that `ci.sh` starts (displays 100–199, a free
loopback port) and stops: windows of other processes on the shared `:7` can take the keyboard
focus from focus-dependent tests. `LEMMA_BROADWAY_DISPLAY=:N` makes it use that display instead.
Extra arguments go to `cargo test`, e.g. `scripts/ci.sh --release`.

## GUI tests never touch the desktop

Tests never open windows on your desktop. Every GUI binary, GUI test and snapshot runs
through **`scripts/broadway.sh <command…>`**:

- it starts (or reuses) an offscreen `gtk4-broadwayd` on display `:7` (`LEMMA_BROADWAY_DISPLAY`
  overrides it);
- it sets `GDK_BACKEND=broadway` and unsets `WAYLAND_DISPLAY` and `DISPLAY`, then `exec`s the
  command, so the measured process is the command itself;
- it sets `GTK_IM_MODULE=gtk-im-context-simple`, GTK's own input method: with an IBus or Fcitx
  module installed GTK would otherwise pick it, talk to the desktop's input method daemon and
  get keys back asynchronously.

To watch a Broadway session, open `http://127.0.0.1:8087` (display `:7` serves on port 8080 + 7).

**GTK test conventions** (§10.1): GTK tests are integration tests named `tests/gtk_*.rs` with
`harness = false` and their own `main`, so `gtk::init()` runs on a single thread. When init
fails they print `SKIP: no display` and exit 0, so `cargo test` without Broadway still passes.
Their harness (`tests/common`) takes test-name substrings as filters (`cargo test -p lemma-app
--test gtk_search -- replace`) and fails a test that makes GLib log an unexpected warning or
critical.

**Programs the M3 tests use** when they are installed (each such test prints `SKIP` and passes
without its program): `pdflatex` with `mylatexformat` (math preview: `lemma-preview`,
`gtk_math_preview`), `chktex` (`lemma-build` `chktex`, `gtk_lint`; its golden depends on chktex
1.7.9 and TeX Live's `chktexrc`), `texcount` (`gtk_lint`'s word count tests) and `git`
(`gtk_git_gutter`, `gtk_session`: throwaway repositories under the test's temporary directory,
with `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1` and `GIT_CEILING_DIRECTORIES`, so no
user or system git configuration and no enclosing repository is read).

**A fake latexmk:** build behaviour that does not depend on TeX is tested without it, with a
shell script as `[build] latexmk` (copied into place with `cp`, so no forked child holds it open
for writing): `gtk_pdf` (the build toasts replace each other; a missing latexmk is explained once,
and again only after a latexmk started: a build that could not execute one does not count, and
the next save withdraws its toast; Clean Build Files and Rebuild stops the build and runs `-c`
with the build's engine flag and output directory, and Stop stops the clean-up) and `gtk_qol`
(auto-saving several documents builds once). `lemma-build`'s `tests/build.rs` runs the real
`latexmk -c` (`Build::clean`) with `outdir` `.build` and `.`, and with a stale `.bbl` and one
that is a source.

**State and settings in tests:** every test window gets `Paths` in a temporary directory (or
none) and an in-memory `Config`; binary runs get `LEMMA_CONFIG=/dev/null` (or a temporary
`config.toml`) and temporary `HOME`/`XDG_*` directories. Runs that quit by themselves never
read or write a session, the palette's recent commands, local history, review state or the
recovery copies of unsaved changes (test windows switch recovery on with
`recovery::set_enabled` only with temporary `Paths`), and keep the math preview's formats in a
temporary directory that goes with the window. Nothing reads or
writes the user's `~/.config/lemma`, `~/.local/state/lemma`, `~/.local/share/lemma` or
`~/.cache/lemma`.

**Runaway logs:** a GTK bug can loop on a warning (a `GtkTextView` disposed with a popover still
parented prints "GtkPopover is not a child of LemmaTexView" forever, 1.6 GB in two minutes). When
you run GTK tests by hand, cap the output (`… 2>&1 | head -c 200000 > target/x.log`) or use a
`timeout`, and keep logs under `target/`.

## The agent: `tools/lemma-fake-agent`

Tests never run `claude`, the ACP adapter or `npm`, and never call a model. The agent in every
test is `lemma-fake-agent` (WP2.3), a hand-written ACP **agent** on stdio that mirrors the
adapter's message shapes (`@agentclientprotocol/claude-agent-acp` 0.89.1): it launches the MCP
server it is given (Lemma's `lemma-mcp` bridge), does Claude CLI's MCP handshake, checks that
Edit/Write/NotebookEdit are disallowed, and plays a **scenario**: a JSON object mapping prompt
keywords to steps (`say`, `think`, `stream`, `plan`, `tool`, `permission`, `disk_write`,
`usage`, `notice`, `sleep_ms`, `stop`, …; the module docs of `tools/lemma-fake-agent/src/main.rs`
list them all). `lemma-fake-agent --check FILE…` validates scenario files.

The scenarios in `tests/fixtures/agent/` work on a copy of `tests/fixtures/proj`:

| Scenario | What it does |
|---|---|
| `basic.json` | two edits in `main.tex`, one in `chapters/yoneda.tex`, then `compile`; prompts with `inline`/`region` make one edit in the abstract (Ctrl+K) |
| `permission.json` | a Bash permission request; `once`/`always`/`reject` fail the turn unless that option is chosen; `plan` sends the ExitPlanMode options |
| `bypass.json` | writes that bypass the tools (temp file + rename, as `sed -i`); `inplace` writes in place |
| `errors.json` | three refused edits with the exact §4.2/§10.4 texts (not found, not unique, outside the root) |
| `cancel.json` | streams, then sleeps 30 s until cancelled; `permission` leaves a request pending |
| `long.json` | 200 message chunks and 30 edits of a new file, for performance |
| `resume.json` | an agent that can load sessions (`_options.load_session`): `polish` edits the abstract; a load replays a conversation marked `REPLAYED` (a thought, a plan, an edit tool call, an answer) that the client must not show again |

How Lemma runs it: `lemma FILE --agent-scenario SCENARIO.json` makes the window's agent the fake
agent (found next to `lemma`, or through `LEMMA_FAKE_AGENT`). Cargo does not build other
packages' binaries for `cargo test -p lemma-app`, so the agent tests call
`common::require_agent_binaries()`, which builds `lemma-fake-agent` and `lemma-mcp` into the
test's target directory first (they fail, not skip, when that is impossible).

Where the agent is tested:

- `crates/lemma-agent/tests/acp_*.rs`: the ACP client against the fake agent (handshake,
  streaming, permissions, cancel, process lifetime, threads, `session/load` with and without
  the agent's `loadSession`: the replay is not reported); `mcp_*.rs`: the MCP server and the
  `lemma-mcp` bridge.
- `tests/gtk_agent_tools.rs` (bridge, tools, watcher, history, the bounded text of the tool
  records; e2e with `basic`, `bypass`, `errors`, `long`) and `tests/gtk_agent_panel.rs` (panel,
  inline edit, quick actions, install branches with a fake `npm`/`claude` on a temporary
  `PATH`; the idle stop on a command channel: what holds an idle agent, `idle_stop_min = 0`
  (also set while the timer runs), a backend that `config.toml` replaced, the API backend; e2e
  with `basic`, `permission`, `cancel`, and `resume`: an idle stop with the pane hidden ends the
  fake agent's process, a start that fails keeps the conversation, and the next message loads
  the same session in a new one, in the mode it had, without the replayed conversation; after
  the project root moved, the next message starts a new conversation and the transcript says
  why first, and a switch of backend says that the other agent starts a new one):
  in-process. `gtk_agent_panel` also types into the prompt box on real key events
  (`prompt_box_on_real_key_events`: a dead key's preedit keeps Enter from sending, and Up and
  Down recall earlier prompts through the text view's own cursor moves; the binary sets
  `GTK_IM_MODULE=gtk-im-context-simple`, since GTK would otherwise pick an installed IBus or
  Fcitx module, which talks to the desktop's daemon and answers keys asynchronously); in
  process, its `prompt_keys_go_to_the_input_method_while_it_composes` emits the text view's
  `preedit-changed` as a CJK input method would, and `streaming_table_updates_in_place` checks
  that a streaming table keeps its grid and cell labels.
- `tests/gtk_e2e_agent.rs`: the real `lemma` binary with `--demo agent` (below).
- `scripts/e2e-agent.sh [case…]`: every scenario through the real binary, offscreen. Each case
  copies `proj` to `target/e2e-agent/<case>/`, runs the demo, checks the report line and the
  files on disk, and keeps `target/e2e-agent/<case>.png`. Cases: `basic`, `inline`,
  `permission` (card left pending), `permission-y`, `permission-Y`, `permission-n`, `bypass`,
  `errors`, `cancel` (Stop within 500 ms), `cancel-permission`, `long`.

**`--demo agent`** sends one prompt (or, with the knobs below, does what the user would) and
prints a report line, `agent-demo: stop=EndTurn turn=1 chunks=6 tools=4 tool_errors=0
permissions=0 cards=- cancelled_ms=- builds=1 build=Success hunks=chapters/yoneda.tex:1,main.tex:2
disk=same view=main.tex:22-42` (`disk=same`: every reviewed buffer equals its file). Knobs, from
the environment:

| Variable | Effect |
|---|---|
| `LEMMA_DEMO_PROMPT` | the prompt text (a scenario keyword picks its steps) |
| `LEMMA_DEMO_ANSWER` | `y`, `Y` or `n`: answer each permission card with that key, through `win.agent-focus` and the panel's key handler; unset, the demo stops at the first card |
| `LEMMA_DEMO_CANCEL_MS` | press Stop (`win.agent-stop`) that long after sending |
| `LEMMA_DEMO_INLINE`, `LEMMA_DEMO_LINE` | send the instruction through Ctrl+K on the paragraph at that 1-based line |
| `LEMMA_DEMO_RETURN_MS` | resolve that long after sending even if the turn still runs (a snapshot of a running turn) |

Runs that quit by themselves (`--quit-after-ms`) write no local history and no review state into
the user's state directory.

### The other backends (M5): API key, Pi and the other ACP agents

No backend's real counterpart runs in a test: no request reaches the Anthropic API, and `pi`,
`codex`, `grok`, `gemini`, `opencode`, `copilot` and their adapters never start.

- **API key** (`lemma_agent::messages`, `keys`): `crates/lemma-agent/tests/messages_backend.rs`
  serves scripted SSE from a std `TcpListener` on 127.0.0.1 through `LEMMA_ANTHROPIC_BASE_URL`
  (streaming, thinking, parallel tool calls, `INVALID_JSON`, `max_tokens` and refusal stops,
  retries on 429/529 with `retry-after`, none on 401, cancel, image results, usage and cost,
  cache control, append-only history, and that the key appears in no log line);
  `messages_keys.rs` uses a fake `secret-tool` script on a temporary `PATH` with
  `ANTHROPIC_API_KEY` unset.
- **Pi and ACP `fs/*`** (`AcpFlavor::Generic`): `crates/lemma-agent/tests/acp_fs.rs` and
  `tests/gtk_agent_backends.rs` play Pi's adapter with `lemma-fake-agent` (`_options.fs`,
  `agent_name`, `session_new_error`, and the `fs_read`, `fs_write` and `fs_edit` steps):
  `fs/write_text_file` edits make the same hunks as MCP edits, `fs/read_text_file` reads the
  live buffer, an auth error shows the login hint. The Pi install branches run with a fake
  `node`/`npm` on a temporary `PATH`.
- **Codex, Grok Build, Gemini CLI, OpenCode, Copilot, custom** (also `AcpFlavor::Generic`):
  `acp_fs.rs` checks the generic mode policy against the fake agent's `_options.modes: "codex"`
  (Codex's modes, no `default`: the elevated `agent-full-access` is never offered and is
  switched back from) and that Lemma's instructions open only the first prompt (`echo_prompt`;
  the fake agent leaves that block out of its keyword search), the next one again when the
  agent answered the first with an error (`_options.prompt_error`), and the first after a
  `session/load`. `install.rs`'s unit tests find
  each tool in a temporary home (`PATH`, `~/.local/bin`, `~/.<tool>/bin`, mise's `npm:`
  installs), a configured `command`, and Codex's node, `codex` and adapter. `gtk_agent_backends`
  runs a Grok turn end to end (instructions, a disk write that becomes a hunk, Codex's modes in
  the picker), lists every backend in the picker with `[agent.<backend>] name`, explains a
  missing tool, installs the Codex adapter with a fake npm, and shows each agent's login hint.
- **Picker and key row** (`gtk_agent_backends`): switching backends (with the confirmation while
  a turn runs) and saving `[agent] backend`, the API key row and its dialog with a fake
  `secret-tool` (the key goes on stdin, never in the arguments), and the cost label. The process
  environment is isolated first: `HOME`, `XDG_*`, `LEMMA_CONFIG` and `PATH` point into a
  temporary directory, so the real keyring and user directories are never touched.

### Real agent (maintainer only)

`LEMMA_DEMO_REAL_AGENT=1` lets `--demo agent` run **without** `--agent-scenario`. It then starts the real Claude adapter,
which uses your Claude login and plan, so **never set it in tests, CI or scripts**.
`LEMMA_DEMO_HANDSHAKE_ONLY=1` stops after `initialize` + `session/new` (no prompt, no model call) and prints the
`Ready` details. `LEMMA_DEMO_TURN_TIMEOUT_S` raises the 120 s turn wait. With the real agent, the report line is
followed by `agent-demo-tool:` lines and an `agent-demo-message:` line.

## Unit, integration and property tests

- Pure crates test with plain `cargo test -p <crate>`.
- Property tests use `proptest`. Each suite documents its default case count; set
  `PROPTEST_CASES=10000` locally for a deeper run (the review core asks for ≥ 2000 by default).
- The review's batched feed (`FileReview::on_edits`) is checked against `on_edit` change by
  change by a differential property in its white-box tests
  (`review::tests::on_edits_is_on_edit_for_each_edit`): random sessions of every kind of step
  on documents of a few KiB, then a random batch (spread out, adjacent, replace_all-like) for
  each origin; every field of the two reviews must be equal but the word spans (a recompute
  has 8 ms for them, which a loaded machine can run out of). Alt+U of an Accept all
  (`unaccept_batch`) is checked the same way against putting the regions back one by one
  (`review::tests::unaccept_batch_is_unaccept_each`). `tests/common`'s `Session::edits`
  goes through `on_edits` too, so every review suite uses it; `gtk_document`
  `replace_all_under_review_reports_every_change` checks `Document::apply_edits` (what the
  review gets, the observers, the line deltas and the version) on a closed and an open document.
  The histories of both run with `play`'s checks of each step, and in the white-box tests every
  undo record is checked against the text after every change (`FileReview::check_records`), and
  `growth_before`'s sum from the end of the hunk list against the sum from the start.
- The buffer's undo and redo of agent edits beyond the records' cap: `tests/properties.rs`
  property 9 (a replace_all of hundreds of lines between smaller agent edits, as a closed
  document and as an open buffer report it; a fortieth of the cases, at least 10),
  `tests/review.rs` `undo_and_redo_of_an_agent_action_beyond_the_record_cap` and `gtk_review`
  `undo_and_redo_of_a_large_agent_edit_keep_it_under_review` (real `undo` and `redo` of the
  buffer).
- Edits that close an open user action move past what its `end-user-action` handlers changed:
  `document::tests::edits_move_past_earlier_changes` (random texts, changes and edits, each
  byte tracked by an id: a moved edit covers the same bytes, or is refused exactly when a change
  went inside it), `gtk_document` `edits_move_past_what_closing_the_user_action_changed` (a
  handler's change, the observers, the review, the refusal, the action open again) and
  `gtk_envtools` `agent_edits_while_a_rename_waits_land_where_meant` (live rename's partner,
  inside a user action left open as Vim's insert mode does).
- `lemma-mcp` has an end-to-end test (`crates/lemma-mcp/tests/bridge.rs`) that runs the real
  bridge binary against an in-test socket server. Unix socket paths must be **shorter than 108
  bytes**, so test sockets live under `$XDG_RUNTIME_DIR` (or a short temp dir), and one test passes
  the bridge a relative path. Do the same in any test that creates a socket.
- `insta` snapshots (lemma-build, lemma-syntax) live in `snapshots/` next to the test. Review
  changes with `cargo insta review` if `cargo-insta` is installed; otherwise run the test with
  `INSTA_UPDATE=always` and inspect the diff in git.
- **Regression tests of the review passes** keep the defect's id in their name:
  `regress_*` (M1: `crates/*/tests/regress_m1.rs`, `gtk_regress_{a,b,c,d}.rs`, `regress_b.rs`) and
  `regress2_*` (M2: `crates/lemma-agent/tests/regress2_a_{acp,mcp}.rs`,
  `crates/lemma-review/tests/regress2_d.rs`, `crates/lemma-complete/tests/regress2_e2.rs`,
  `crates/lemma-app/tests/regress2_c.rs`, `gtk_regress2_{b,c,d,e1,e2,f}.rs`). The only ignored
  ones are `regress2_d`'s release timing tests: `regress2_replace_all_20k_matches_is_fast`
  (D-5, fixed: about 0.1 s there but 4 s in a debug build; run it with `cargo test --release -p
  lemma-review --test regress2_d -- --ignored regress2_replace_all`) and
  `perf_reject_all_of_5000_hunks_and_its_undo` (see Performance tests). The M3 integration's
  regression test is `gtk_agent_panel` `regress3_inline_popover_of_a_closed_tab_leaves_no_critical`;
  its other fixes are checked by assertions added to existing tests. The M5 review's 16 are
  `regress5_*`, next to the code they test: unit tests in `bibimport.rs`, `format.rs` and `editor/align.rs`; `crates/lemma-agent/tests/regress5_acp_env.rs`
  (the adapter is `/bin/sh`), `messages_backend.rs` (`regress5_a_*`, the fake API server);
  `gtk_app`, `gtk_agent_backends`, `gtk_envtools` and `gtk_qol`.
- **M3 tests:** one GTK test binary per feature (`gtk_outline`, `gtk_goto`, `gtk_labels`,
  `gtk_math_preview`, `gtk_pdf_m3`, `gtk_palette`, `gtk_prefs`, `gtk_search`, `gtk_lint`,
  `gtk_session`, `gtk_git_gutter`, `gtk_pairs`) plus `gtk_shell_m3` for the shell hooks and the key
  rules, and crate tests in `lemma-preview`, `lemma-search`, `lemma-pdf` (`tests/m3.rs`) and
  `lemma-build` (`tests/chktex.rs`, `tests/explain.rs`). `gtk_pdf_m3` needs
  `target/fixtures/pdf50.pdf` (`scripts/build-fixtures.sh`) for some tests and prints `SKIP`
  without it. Keys are tested on real key events (private Broadway daemons) in `gtk_shell_m3`,
  `gtk_palette`, `gtk_pairs`, `gtk_outline`, `gtk_pdf_m3` and `gtk_agent_panel` (the prompt
  box); pointer hovers through the modules' `pointer_moved` functions (GTK cannot inject
  pointer events).
- **Start-up and sessions:** `gtk_session`'s `session_restores_tabs_in_the_background` (no tab
  but the shown one is selected, focused or spell-checked during a restore),
  `startup_opens_several_files_and_a_line` (`app::open_documents`: the first FILE shown, the
  others behind it, `+LINE` and `FILE:LINE:COLUMN` winning over the session's place, the root
  kept when the first does not open), `startup_shows_a_blank_page_while_opening`,
  `session_panes_before_the_first_frame` and `session_remembers_the_window_size` (`last.json`'s
  window size, maximized under Broadway too); `gtk_app`'s `several_files_and_lines` and
  `usage_errors_and_help` run the binary. The command line's parsing (with files on disk) and
  the sweep of old session files are unit tests in `cli.rs` and `session.rs`.
- **Recovery of unsaved changes** (`recovery.rs`): unit tests for the file format, the
  decision on a file found when a document opens, the bar's texts, the copy delays, the writer
  (the newest operation per file wins, the process's lock), a running writer told by its lock,
  and the sweep; `gtk_session`'s
  `recovery_copies_unsaved_text_until_it_is_saved` (after the delay, while typing goes on, on
  focus loss; removed by a save, `:q!` and turning `[editor] recovery` off; nothing in a test
  window until switched on), `recovery_offers_reviews_and_discards` (the bar's texts, Review as
  hunks of a "Recovered changes" turn that a reject undoes, Discard, a corrupt file ignored, a
  matching one removed, another running Lemma's copy left alone until its lock goes),
  `recovery_restores_tabs_never_saved` (the session brings back the tab of a New File whose
  copy a crash left, only with recovery on) and `recovery_survives_sigterm_and_sigint`, which
  runs the real app (`app::run`) in a child process (`LEMMA_RECOVERY_CHILD`) with temporary XDG
  directories, types into it, sends `SIGTERM` (and `SIGINT`) and checks that the window closed,
  the process ended successfully and the recovery file holds the typed text, which the next
  window offers.
- **The find bar** (`find_bar.rs`, find and replace in the current file): `gtk_search`'s
  `find_bar_*` tests (counts and wrapping, the options and invalid expressions, steps before
  the search has counted looked for in the background, the steps and Replaces after them
  following in order, the actions following the focus and Vim, the prefill and the query a
  match keeps, Replace and Replace All as one undo step, replacements as user edits under
  review and without live environment rename, tab switches and closing tabs and windows freeing
  the search), and its keys on real key events in `gtk_shell_m3` (Ctrl+F shared with the PDF's
  search, Vim's Ctrl+F and Ctrl+H, the Search page's own Ctrl+H). `LEMMA_TEST_SNAPSHOTS` adds
  `find-bar.png` and `find-bar-narrow.png` (the rows wrapped).
- **Go to Line, New File and Save As:** `gtk_palette`'s `quick_open_goes_to_lines` (`:N`,
  `:N:C`, `file:N`, the clamping, Back) and its real keys (Ctrl+G, the digits typed after the
  `:`, Ctrl+N in a picker is its Down); `gtk_window`'s `new_file_*` and `save_as_*` tests, with
  the file dialog answered by `file_commands::set_dialog_hook` (the dialog's folder and name, the
  tab's place, encodings and line breaks, the refusals); `gtk_vim`'s `:w path`, `:wq path` and
  `:sav[eas][!]`; `GotoTarget::LineColumn` in `gtk_shell_m3`, whose real keys check Vim's Ctrl+G
  and Ctrl+N and Ctrl+G in the PDF's search entry; `gtk_keys`, Ctrl+N with Vim in every mode.
- **The project index and the disk:** `gtk_labels` `undefined_follows_files_changed_on_disk`,
  `gtk_completion` `the_project_feed_follows_files_on_disk` and `gtk_outline`
  `a_chapter_changed_on_disk_updates_the_tree` write files with `std::fs` as another program
  would and then wait only for the result: no build, no `completion::refresh_project` and no
  `outline::is_settled` (which looks at the page again by itself), so only the file monitor
  and the project feed can bring it. `gtk_completion` also checks that a save lists no files
  again and, in `the_project_feed_follows_files_on_disk_at_its_cap`, that a changed file is
  read again at the cap of 300 files read from disk without letting more files in (40
  distribution packages in the preamble take no place), that a build changing only the `.aux`
  and a chapter written again unchanged collect no words (`completion::base_word_builds`)
  while a chapter saved in its tab and closed, or saved closed, has the words of its file
  collected on the next build's pass, and, in `the_project_feed_reads_local_packages`, that a local `.sty` (and what it loads) and the
  root's `.cls` reach the index and completion and leave it with their `\usepackage`;
  `gtk_outline` `theorems_declared_in_a_preamble_file` that a preamble's (or a local package's
  or class's) `\newtheorem` names the rows of chapters read before it, and of an open chapter
  whose own re-index runs while the declaration is merged (its index comes out the same, yet
  it is indexed again with the new name), `gtk_goto` that F12 on a
  local package's macro goes to its `.sty`, and `gtk_labels`
  `undefined_shows_without_waiting_for_the_words` that the check runs while the words timer
  still waits (in a release build, under 1.2 s after the edit). `gtk_completion`'s test at the
  cap also checks that a build's pass over the 40 distribution packages makes the main thread
  resolve no path on the file system (`workspace::normalizations`, a per-thread count of
  `normalize`), and `the_project_feed_follows_files_outside_the_folder` writes a bibliography
  and a chapter outside the workspace folder (`../central/refs.bib`, `\input{../shared/intro}`)
  as another program would: changed, deleted and written again, they reach the index through
  the directories the file monitor watches for it (`watcher::watched_outside`), which go when
  the `\input` does; of 40 such directories the first 32 are watched.
- **The Outline and typing:** `gtk_outline` `prose_typed_recomputes_nothing_a_caption_its_row`
  types words into a paragraph with a pause after each (the document is indexed again each
  time) and checks that the outline is not recomputed (`outline::stats(…).syncs`); two lines
  deleted and put back before the index catches up leave no row on a shifted line; a caption
  of a float without a label (only the structure pass records it, `EnvPair::caption`) and an
  emptied short form update their rows. `lemma-syntax` `structure_index` `float_captions`
  checks what the structure pass records (subfigures, `\caption*`, an empty short form,
  verbatim).
- Some GTK tests need real key events and start their own private Broadway daemon in a child
  process (`tests/broadway_input/`); others create second windows to make a window inactive
  (`gtk_regress2_f`). A test that loops on a GLib warning can write gigabytes of output: keep
  test logs under `target/`, not in `/tmp` (a tmpfs with a quota).
- **Main-loop stalls:** the tests that bound one main-loop iteration (`gtk_regress2_b`'s
  `regress2_list_files_does_not_stall_the_main_thread`, 50 ms, and
  `regress2_big_file_written_in_a_turn_does_not_freeze`, 100 ms) time it with
  `common::timed_iteration`: the wall time less the time the main thread waited for a CPU
  (`/proc/thread-self/schedstat`; the wall time where there is none), which a loaded machine
  adds and nothing in Lemma causes. Work and blocking count in full. The window draws a frame
  before the measurement starts (`testmode::wait_for_frames`): a debug build takes 45–60 ms for
  the first one under Broadway.

## Performance tests

Performance tests are `#[ignore]`d and their names start with `perf_`. Run them in release mode,
one at a time, the GTK ones under Broadway:

```sh
scripts/broadway.sh cargo test --release --workspace -- --ignored perf --test-threads=1
cargo test --release -p lemma-syntax -- --ignored perf
```

The GTK test binaries (`harness = false`) add their `perf_*` tests only with `--ignored` and
check the budgets only in release builds:

| Test | Budget |
|---|---|
| `gtk_highlight` `perf_highlight_keystroke_large_tex` | highlighting cost per keystroke on `large.tex` ≤ 0.5 ms p99 |
| `gtk_typing` `perf_typing_keys` | typing-helper keys on a 5k-line document ≤ 2 ms p99 |
| `gtk_typing` `perf_typing_large_tex` | Enter and `\end` keys (which scan environments) on `large.tex` ≤ 2 ms p90 |
| `gtk_completion` `perf_completion_keystroke` | keystroke to frame with completion, autosnippets and highlighting on a 5k-line document ≤ 8 ms p99 |
| `gtk_completion` `perf_completion_end_large_tex` | the proposals after each key of `\end{equation` near the end of `large.tex` (they look for the open environment) ≤ 3 ms p90 (measured 0.1 ms; 3.7 ms when each key copied the document) |
| `lemma-complete` `perf_complete` | completion ≤ 3 ms p99 with all tables and a 7k-line project index |
| `lemma-complete` `perf_word_list_project` | the words of a generated 300-file project (120k lines, 26k words) added at once: linear, twice the files ≤ 2.5× the time (measured 0.16–0.19 s; 0.7–0.9 s when each file re-sorted and copied the list) |
| `agent::markdown` `perf_render_long_message` | re-rendering a streaming 100 kB answer ≤ 2 ms per frame |
| `lemma-agent` `perf_text_deltas_append_in_place` | the API backend's streamed deltas appended to their block: 32k deltas of 16 bytes ≤ 20 ms, and twice the deltas ≤ 3× the time (measured 0.8 ms; 230 ms when every delta copied the block) |
| `gtk_agent_panel` `perf_streamed_table` | a 60-row, 5-column table streaming in 10-byte chunks: each chunk's re-render and layout of the table ≤ 2 ms p99, and the table is never built again |
| `lemma-syntax` `perf_index_pathological` | `index_file` on 14 pathological shapes (unclosed groups, stray `\end`, unterminated `\iffalse`, …) stays linear: twice the size ≤ 3× the time, ≤ 200 ms per MB |
| `gtk_outline` `perf_outline_keystroke` | the outline's work per keystroke on the 7k-line corpus (first 7000 lines of `large.tex`) ≤ 1 ms (p99 and mean); `perf_outline_large_tex` prints the first outline and a patch on `large.tex` and the first outline of 2000 floats, and checks that prose typed with three pauses recomputes neither (before: 3 recomputations, 16 ms in all on `large.tex`, 87 ms with the floats) |
| `gtk_pairs` `perf_pairs_cursor_moves_large_tex` | delimiter-pair highlight per cursor move on `large.tex` ≤ 0.5 ms p99 |
| `gtk_git_gutter` `perf_git_gutter_keystrokes_large_tex` | the git gutter's main-thread cost per keystroke on `large.tex` (with against without the gutter) and per diff |
| `gtk_search` `perf_search_10k_frame_budget` | the Search page's work per main-loop step while 10 000 matches stream in ≤ 4 ms |
| `gtk_search` `perf_find_bar_replace_all_large_tex` | the find bar's Replace All of 20 000 matches on `large.tex`, and what the same line index updates take alone (printed) |
| `gtk_search` `perf_find_bar_replace_all_under_review_large_tex` | the same Replace All with 5000 hunks under review and without a review, and the recompute after it (printed) |
| `gtk_search` `perf_find_bar_step_before_the_scan_large_tex` | F3, Shift+F3 and Enter right after a new query, before the search has counted, on `large.tex` and its first 10 000 lines: the key's own work ≤ 2 ms on 10 000 lines; prints the longest main-loop step until the match and the key once counted |
| `lemma-review` `perf_unaccept_accept_all` | Alt+U of an Accept all of 5000 hunks ≤ 100 ms |
| `lemma-review` `perf_on_edits_replace_all` | a replace_all fed to a review at once (`on_edits`): 20 000 matches ≤ 0.5 s and linear (≤ 6× the time of 10 000); 20 000 over 5000 hunks, or right after accepting or rejecting them, ≤ 0.5 s |
| `lemma-review` `perf_undo_and_redo_of_a_replace_all` | the buffer's undo and redo of a replace_all of 5000 matches in an open buffer (10 000 changes, each recognized by its record), with their recomputes: ≤ 1 s each, every hunk back (measured 0.14 s and 0.21 s) |
| `regress2_d` `perf_reject_all_of_5000_hunks_and_its_undo` | Reject all of 5000 line hunks, its Ctrl+Z and its Alt+U, each with its recompute: ≤ 0.5 s each (measured 0.05 s, 0.07 s, 0.18 s; 2.3 s, 100 s and 1.1 s when each step re-checked every record) |
| `lemma-core` `perf_line_index_replace_all_large_tex` | the line index through the 40 000 changes of a replace all of 20 000 matches in `large.tex`, from the end and from the start, ≤ 50 ms each |
| `gtk_document` `perf_apply_edits_replace_all` | `apply_edits` of 20 000 matches in a file under review: ≤ 0.5 s closed, ≤ 1.5 s open |
| `gtk_session` `perf_recovery_copy_large_tex` | the recovery copy's main-thread cost: ≤ 2 ms for 10k lines (a copy can come due while typing goes on), ≤ 8 ms for `large.tex` (measured 0.9 and 4.5 ms in a dev build: the copy is GTK's), and ≤ 100 µs of a keystroke |
| `gtk_lint` `perf_problems_long_list` | replacing 3000 build problems in five files, the `set` call and the next frame's work: ≤ 60 ms with the Problems page hidden (no rows built), ≤ 300 ms with it shown (500 rows and the line counting the rest) |
| `gtk_math_preview` `perf_math_preview_hover_latency`, `lemma-preview` `perf_render_latency` | hover to rendered formula, p50 ≤ 250 ms with a built format (needs `pdflatex`; sensitive to load: run on a quiet machine) |
| `lemma-pdf` `perf_tiled_page` | a stress.pdf page too large for one bitmap (scale_q 26, 1989 × 2574 px) in its 5 full-width bands through the PDF thread costs under half of the same page in 24 tiles of 512², rendered with poppler as the thread does; the whole page is printed too (measured 23–29, 77–93 and 10–12 ms). Zoomed in far (scale_q 96, 7344 × 9504 px), the 9 tiles of about 1024² under a 1800 × 2000 px view cost under two thirds of the 25 tiles of 512² there (measured 31–34 and 77–78 ms) |
| `gtk_math_preview` `perf_math_preview_large_tex` | on `large.tex` with the live preview (needs `pdflatex`): the main loop's work after the card appears or goes away ≤ 20 ms median (measured under 0.5 ms; 1.2 s while its tag was removed from the whole text, which GTK lays out again), and the hover of a rendered formula right after a one-character edit ≤ 2 ms median |
| `gtk_labels` `perf_equation_numbers_large_tex` | the inline equation numbers of `large.tex` with 5000 labels numbered by the `.aux`: a refresh ≤ 15 ms median (measured 8 ms; 49 ms with a search per label), and the main loop's work after a new undefined reference is underlined ≤ 50 ms |
| `gtk_review` `perf_review_many_hunks` | 3000 one-line hunks in a 9000-line file: the overlay's sync ≤ 15 ms median (measured 10 ms; 85 ms while every hunk searched the layout cache and every tag was searched for in the text), the recompute after typing ≤ 40 ms, the gutter's marks in a frame ≤ 1 ms; prints a line break above the hunks and accepting one (with the main loop's work after it) |

`perf_completion_keystroke` lays the document out before it measures and reports the window
frames Broadway renders meanwhile separately (about 40–50 ms each in software, about once a
second; they are not counted), and prints the slowest keys by phase (key, main loop, proposals,
frame).

Their thresholds come from `docs/ARCHITECTURE.md` §7 and §10.7. They read
`tests/fixtures/large.tex` (60k lines) and, when `LEMMA_PERF_CORPUS` points at a directory of
`.tex` files (your own corpus), those files too.

## Golden images

GUI rendering tests compare a PNG from `lemma --snapshot` against a committed golden image:

- the test passes when the **mean absolute difference is below 2/255** per channel;
- `UPDATE_GOLDEN=1` rewrites the golden images instead of comparing; check the new PNGs before
  committing them;
- goldens are rendered under Broadway, so they never depend on the desktop's scale or theme.

`--snapshot PNG` renders the window's `WidgetPaintable` with a `gsk::CairoRenderer` realized
without a surface, after the first frame has been drawn, so it works on every backend.

## Screenshots: `scripts/shots.sh` and `scripts/shot.sh`

`scripts/shots.sh [name…]` regenerates every documentation screenshot (the `all` list in the
script) into `target/shots` (release build by default; `LEMMA_PROFILE`, `LEMMA_BIN`, `LEMMA_SHOTS_DIR`,
`LEMMA_SHOT_MS`), on copies of the fixtures in a temporary directory under `target/`, the agent
shots with the fake agent; every run gets `LEMMA_CONFIG=/dev/null` (`m-prefs`: an empty
writable `config.toml`) and a temporary `XDG_STATE_HOME`. The M3 shots use the app's `--demo`s and
their `LEMMA_DEMO_*` knobs; `p-git-gutter` commits a copy of `proj` into a
throwaway repository first. `b-review-turns.png` comes from `tests/gtk_review_persist.rs` with
`LEMMA_TEST_SNAPSHOTS`, and so do the `t-*` shots (into `target/shots/tests/`):
`t-history-timeline` (`gtk_session`), `t-lint` (`gtk_lint`), `t-labels` (`gtk_labels`) and
`t-pairs` (`gtk_pairs`); M5 adds `t-envtools` (`gtk_envtools`) and `t-agent-backends`
(`gtk_agent_backends`: the backend picker and the API key row and dialog), and `r-templates`
from `--demo templates`. Other GTK tests write review snapshots with `LEMMA_TEST_SNAPSHOTS=dir`
too (their module docs name them: `gtk_shell_m3`, `gtk_goto`, `gtk_math_preview`, `gtk_pdf_m3`,
`gtk_palette`, `gtk_prefs`, `gtk_search`, `gtk_git_gutter`). Look at every image after a change
that affects the UI.

One screenshot:

```sh
scripts/shot.sh /tmp/m0.png tests/fixtures/proj/main.tex
```

Builds `lemma-app` (or uses `LEMMA_BIN`) and runs
`lemma <args> --snapshot OUT --quit-after-ms 1500` under Broadway. Paths are relative to the
current directory. `LEMMA_SHOT_MS` changes the delay, and `LEMMA_PROFILE=release` builds a release
binary.

`LEMMA_SHOT_WAYLAND=1` instead launches lemma on the real Wayland session and grabs its Hyprland
window with `grim`. That opens a window on your desktop, so use it only by hand.

**Scripted runs** combine these steps (in this order) before the snapshot:
`--demo review|pdf`, `--apply-as-agent JSON`, `--build` (builds as F5 does, waits until the PDF is
rendered, and after TeX errors puts the cursor on the first error) and `--sidebar
outline|changes|problems`. `--apply-as-agent` and `--build` write files, so run them on a copy of a
fixture, never on `tests/fixtures` itself:

```sh
cp -r tests/fixtures/logs/undefcs "$T/" && \
  scripts/broadway.sh lemma "$T/undefcs/main.tex" --build --sidebar problems \
  --snapshot target/shots/problems.png --quit-after-ms 2000
```

All scripted-run options (the steps run in the order demo, agent edits, build, sidebar, memory
report, snapshot; runs that quit by themselves write no local history or review state):

| Option | What it does |
|---|---|
| `--snapshot PNG` | renders the window to a PNG after the other steps |
| `--quit-after-ms N` | quits N ms after start; only such runs close without asking about unsaved changes |
| `--mem-report` | prints `Rss:`/`Pss:` from `/proc/self/smaps_rollup`, before any snapshot |
| `--demo review` | opens a demo document in a temp directory and applies four agent edits (run it without FILE) |
| `--demo pdf` | shows the last build's PDF, fills the page cache and shows 3 pages |
| `--demo agent` | one agent turn with the fake agent (needs `--agent-scenario`); prints an `agent-demo:` report line |
| `--demo completion` | the completion popup after `\fr` in math (a demo document without FILE) |
| `--demo outline` | the Outline page of FILE's project, the cursor in its first numbered theorem (`LEMMA_DEMO_FILTER=text` filters) |
| `--demo hover-ref` | the hover of the first `\eqref`/`\ref` (with its PDF crop); `LEMMA_DEMO_HOVER=0`: the same place with its equation numbers |
| `--demo math-preview` | the rendered formula in the hover (`LEMMA_DEMO_FORMULA=needle` picks one) |
| `--demo pdf-search` | the PDF of FILE's last build searched for "nulla" (`LEMMA_DEMO_PDF_QUERY`; `LEMMA_DEMO_PDF_STRIP=thumbnails\|bookmarks` shows the strip) |
| `--demo palette` | the command palette with "pdf" typed |
| `--demo search` | the Search page: "sheaf" with the replace preview "faisceau" (`LEMMA_DEMO_SEARCH`, `LEMMA_DEMO_REPLACE`) |
| `--demo prefs` | Preferences, first page (needs a writable `LEMMA_CONFIG` to show the controls enabled) |
| `--demo pairs` | a `\begin`/`\end` pair highlighted (`LEMMA_DEMO_PAIR=needle` puts the cursor elsewhere) |
| `--agent-scenario JSON` | the agent is `lemma-fake-agent` playing this scenario (`tests/fixtures/agent/`), never Claude |
| `--apply-as-agent JSON` | applies the edits in JSON as agent edits, as the MCP tools would (format in `testmode/review.rs`) |
| `--build` | builds as F5 does and waits for the result; after TeX errors, the cursor goes to the first error |
| `--sidebar outline\|changes\|problems\|search` | shows that sidebar page at the end |
| `--restore-session` | restores the project's session in a scripted run too, and without a FILE reopens the last session's project and file (scripted runs never restore otherwise, and never save one) |

Scripted runs turn GTK animations off, so a snapshot never catches a transition halfway.
`testmode::snapshot_png` places a popover so that its visible part touches what it points at,
as a compositor does (Broadway would place the whole popup surface, shadow included). Broadway's
screen is 1024×768, which is below the 1100sp breakpoint: there the sidebar is an overlay over the
editor. The app follows the system style; `ADW_DEBUG_COLOR_SCHEME=prefer-light` forces light.

## Fixtures

Committed fixtures (`tests/fixtures/`, never written to; TeX builds run in temp dirs or under
`target/`):

| Path | Contents |
|---|---|
| `proj/` | `main.tex`, `refs.bib`, `chapters/yoneda.tex` and the built `main.pdf`, `main.synctex.gz`, `main.aux`, `main.log`, `main.blg`. The synctex file records the absolute paths of the directory it was built in, so tests compare path **suffixes** (`…/main.tex`, `…/chapters/yoneda.tex`). |
| `logs/<variant>/` | `main.log`, `main.79.log` (79-column wrapping), `*.blg`, `run1.log`, `latexmk.out`, and the `.tex`/`.bib` sources of each broken build (pdfLaTeX, LuaLaTeX and XeLaTeX variants). |
| `pdf/` | `stress.pdf` (23 pages), `test.pdf` (3 pages) and its `test.tex`. |
| `large.tex` | 60k lines, for performance and memory tests. |

Crate-local test data (M3): `crates/lemma-build/tests/data/explain/` (error messages for the
explanation table: one `main.tex` per error and the `main.log` of a real run of
`pdflatex -interaction=nonstopmode -file-line-error` with `max_print_line=10000 error_line=254
half_error_line=238`, as Lemma's builds set them) with the chktex golden `crates/lemma-build/tests/snapshots/
chktex__fixtures.snap`, and `crates/lemma-pdf/tests/golden/proj-outline.txt` (the bookmark tree
of `proj/main.pdf`).

Generated fixtures (`scripts/build-fixtures.sh`, output in `target/fixtures/`, idempotent):

| Path | Contents |
|---|---|
| `pdf50.tex`, `pdf50.pdf`, `pdf50.synctex.gz` | 50 A4 pages, one section per page: lipsum text, an `align` display, a matrix and hyperref cross-references to the previous page. The script fails unless `pdfinfo` reports exactly 50 pages. |
| `doc1k.tex` | The first 999 lines of `large.tex` plus `\end{document}`: the 1k-line idle memory scenario. |

Bibliography fixtures for `scan_bib` come from the system TeX Live:
`kpsewhich font.bib tugboat.bib`.

`data/raw/` (unicode-math tables and TeXstudio cwl files) is the input of
`cargo run -p xtask -- gen-completion`, not a test fixture.

## Memory budgets: `scripts/rss.sh`

```sh
scripts/rss.sh            # every scenario in scripts/budgets.toml
scripts/rss.sh pdf50      # one scenario
```

Each scenario in `scripts/budgets.toml` lists the lemma arguments, a run time, and RSS/PSS
budgets from §7. The script runs `lemma <args> --mem-report --quit-after-ms <ms>` (release build
by default; `LEMMA_PROFILE`, `LEMMA_BIN` override it) with `LEMMA_CONFIG=/dev/null` (the
defaults, spell checking on; set `LEMMA_CONFIG` to measure another configuration) and a
temporary `XDG_STATE_HOME` and reads the **last `Rss:` and `Pss:`
lines in kB**, the `/proc/self/smaps_rollup` format, from the app's stdout or stderr. So
`--mem-report` must print at least:

```
Rss:              98500 kB
Pss:              45000 kB
```

It reads `smaps_rollup` before any snapshot, because rendering a texture adds about 60 MB.

- **Default (Broadway):** the numbers are printed but not asserted, because Broadway's memory
  profile differs from Wayland's.
- **`LEMMA_RSS_WAYLAND=1`:** runs on the real Wayland session and fails when a value exceeds
  budget × `tolerance` (1.15). It opens windows on the desktop, so run it by hand only.

Scenarios that use `target/fixtures/*` need `scripts/build-fixtures.sh` first.

## Safety rules for tests

- No network beyond what cargo itself does; tests never call a model or any paid API. Agent tests
  use `tools/lemma-fake-agent` (M2).
- The API-key backend and the BibTeX import are tested against local fake servers on 127.0.0.1
  (`LEMMA_ANTHROPIC_BASE_URL`, `LEMMA_DOI_URL`, `LEMMA_ARXIV_URL`); never set them to a real
  host in a test. Key storage uses a fake `secret-tool`, never the user's keyring. latexindent
  formatting is tested through a fake script (`format::set_program`).
- Tests write only to temp dirs, `$XDG_RUNTIME_DIR` (sockets) or `target/`.
- No test starts another application: the PDF's Open in Another App and Show in Files go to
  `PdfController::set_launch_hook` (`gtk_pdf`), web links in the PDF to `links::set_uri_hook`,
  and scripted runs refuse both.
