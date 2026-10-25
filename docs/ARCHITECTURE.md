# Lemma: architecture

The design as planned before implementation (v1.0, 2026-10-10). Section 11 lists what changed
since; where they conflict, section 11 wins.

Legend for claims:
- **[V]** built, run or measured while the design was made.
- **[S]** read in the source but not executed.
- **[U]** unverified. The work package that depends on it must check it first.


## 0. Summary and changes from the proposals

Lemma is a GTK4 + libadwaita desktop LaTeX editor written in Rust. It has five main parts:
- a GtkSourceView editor with an optional Vim mode;
- our own LaTeX scanner, which drives highlighting, spell-check exclusion, snippets and completion;
- a poppler PDF preview with SyncTeX;
- latexmk builds with log diagnostics;
- Claude as an agent over ACP. Every agent edit lands in the buffer immediately and is shown as a red/green hunk that the user can accept or reject on its own.

The design is based on the **lean** proposal: no tokio, a hand-written MCP server, everything loaded lazily. I grafted in ideas from the other proposals and fixed the judge's findings:

1. **Review comes first.** The review core and its inline rendering are in M1, driven by `lemma --apply-as-agent edits.json`. The ACP agent follows in M2. The pure typing crates (scanner, snippets, structure index) are built in parallel in M1.
2. **Snippets are built chunk by chunk and never parsed.** I verified two problems with GtkSourceView 5.20's `Snippet::new_parsed`:
   - a bare `$1` produces no tab stops;
   - `\$` loses the dollar sign. This is new; it breaks the `mk` snippet `$$1$$0`.

   Chunks are built as follows, all **[V]**:
   - literal text: `set_text` + `set_text_set(true)`;
   - a tab stop: `set_focus_position(n)`;
   - a default: `set_text` on the stop chunk. It is pre-selected and replaced by typing;
   - a mirror: `set_spec("$n")` with focus −1.

   GtkSourceView does **not** indent snippet continuation lines, with or without `auto_indent` **[V]**, so we add the indentation ourselves.
3. **Correction to the judge.** With a language set and `set_highlight_syntax(false)`, GtkSourceView still creates *and applies* `gtksourceview:context-classes:no-spell-check` (2 ranges in the probe) **[V]**. So once our highlighter lands in M2, `.tex` buffers get **no language**. M0–M1 use the built-in `latex` language.
4. **Disk and agent safety:**
   - Every agent edit is written to disk *before* the tool call is answered.
   - ReviewEvents ride on the next tool result during a turn and on a `<review_feedback>` block before the next prompt.
   - Every path is canonicalized; anything outside the project root is refused.
   - The MCP socket uses a token, and a bad token is refused.
   - Tool calls time out after 30 s; `compile` gets the build timeout plus 20 s.
5. **Review UI:**
   - Each hunk gets a header band in reserved space, served from a pool of 8 overlay bars.
   - Hunks the user has edited are marked amber.
   - Alt+D switches to a reading mode that hides deleted lines.
   - The cursor advances to the next hunk after each decision.
   - An `adw::Banner` summarizes the review, and the Changes panel has a plain-text diff for accessibility.
6. **Undo:** a `History` origin is detected through the TextBuffer `undo`/`redo` signals. Ordering was verified: the before-handler runs before the change, and Vim `:u` emits `undo`. A reject journal makes undoing a Reject re-open the hunk.
7. **Smaller fixes:**
   - **frizbee 0.13.0** (zero dependencies, blink.cmp's matcher) is used instead of nucleo.
   - The GTK stack is pinned with `=`.
   - The adapter is installed with `npm --omit=optional`: 64 MB instead of about 314 MB, and it reads `CLAUDE_CODE_EXECUTABLE` first [S]+[V].
   - Corrected claim: the 144 MB "60k-line probe" measures loading the whole buffer (text B-tree, layout and highlighting together), not highlighting alone.

**Also verified in this pass [V]:**
- **Dependencies:** the §10 workspace resolves to 248 locked packages. The app pulls 193 runtime crates with a single `glib` 0.22.10 and no duplicate runtime versions. The app links against the system gtksourceview-5 and libspelling.
- **Spell check:** libspelling skips text carrying our own `no-spell-check` tag, whether the tag is created before or after the adapter (after it, call `invalidate_all`).
- **Gutter:** a `GutterRenderer` subclass compiles and inserts. Its wrapper needs `@implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget`, and `gutter()` must be called as `sourceview5::prelude::ViewExt::gutter(&view, …)` because it is ambiguous with `TextViewExt`.
- **ACP agent thread:** the pattern compiles against agent-client-protocol 3.3.0: a map of pending permission responders, a spawned task per prompt, and Cancel answering every pending permission request.
- **PDF:** the path poppler on a worker thread → `ImageSurfaceDataOwned` → `glib::Bytes::from_owned` → `MemoryTexture` works, and so does `link_mapping`.
- **MCP bridge:** `lemma-mcp`, std-only and 369 KB in release, relays JSON-RPC lines and refuses a bad token. Unix socket paths must be **shorter than 108 bytes**, so tests must use relative paths or `$XDG_RUNTIME_DIR`.
- **Vim commands:** calling `VimIMContext::execute_command(":w")` from code emits neither `write` nor `execute-command`. Whether the interactive command bar emits them is **[U]**; see WP0.2.
- **Other APIs:** the frizbee and similar (`timeout`, `iter_inline_changes`) APIs, pulldown-cmark without default features, libc `M_MMAP_THRESHOLD`/`M_ARENA_MAX`/`malloc_trim`, `glib::user_state_dir`.

## 1. Stack decisions

| Concern | Decision | Justification |
|---|---|---|
| GUI | gtk4-rs 0.11.5 + libadwaita 0.9.2, plain GObject subclassing, no relm4 | Lowest memory of the stacks measured: 98.5 MB RSS / 45 MB PSS for the editor alone, 110/56 MB with 3 PDF pages [V]. Uses system libraries and builds clean in about 1 minute. |
| Editor widget | `TexView`, a subclass of `sourceview5::View` (GtkSourceView 5.20, feature `v5_18`) | Brings undo, Wayland IME, VimIMContext, a completion popup, the snippet engine, regex search and marks [V]. |
| Highlighting | M0–M1: built-in `latex` language. From M2: `.tex` buffers have **no language**, and `lemma-syntax` tokens set TextTags styled from the active StyleScheme (`StyleScheme::style(id)` + `Style::apply(&tag)` [S]). The same pass applies `no-spell-check`. `.bib` keeps the built-in `bibtex` language. | One definition of math mode for highlighting, spell exclusion, snippets and completion. |
| Parsing | Our own incremental scanner (`lemma-syntax`) | 2 ms for a 7k-line file. tree-sitter-latex took 230 ms and about 27 MB, and flags `\item[(i)]` as an error [V]. |
| PDF | poppler-rs 0.26.0 on one worker thread | System library; 4–11 ms per page; `from_bytes`, `link_mapping` and `find_text` available [V]. |
| SyncTeX | Hand-written FFI to the system libsynctex 2.0.0 (13 functions) | About 50 µs per query in-process [V]. |
| Agent protocol | ACP: `agent-client-protocol =3.3.0` with feature `process`, driven by `futures_executor::block_on` (no tokio) [V] | Typed and maintained; the same client works for Codex and Gemini later. |
| Agents and auth | Claude through `@agentclientprotocol/claude-agent-acp@0.89.1`, installed with `--omit=optional` into `~/.local/share/lemma/acp`, with `CLAUDE_CODE_EXECUTABLE` pointing at the absolute path of `claude`. It reuses the existing Claude Code login. M5 adds an API-key Messages-API backend. | No API key needed. **Personal use only:** Anthropic's Agent SDK policy forbids offering claude.ai login in distributed products. |
| Edit routing | `_meta.claudeCode.options = {disallowedTools:[Edit,Write,NotebookEdit], allowedTools:["mcp__lemma__*"]}` plus `_meta.systemPrompt.append`. `toolAliases` is not used. | The adapter never calls `fs/write_text_file` [S]. |
| MCP | **Yes, from M2.** A hand-written JSON-RPC server in `lemma-agent` on a unix socket (directory mode 0700, plus a token). The CLI spawns `lemma-mcp`, a std-only stdio↔socket bridge. In M5 the same bridge is documented for Claude Code running in a terminal. | Avoids rmcp + tokio + axum (about 100 crates). The handshake was verified with CLI 2.1.288 [V]. |
| Completion data | L1: static tables generated by xtask and committed (curated kernel and common packages, plus 823 unicode-math symbols mapped to pdflatex commands). L2 (M4): TeXstudio `.cwl`, one zstd frame per package, decoded with ruzstd only when `\usepackage` appears. L3: the project's macros, labels and bib keys. | Costs nothing until used. Loading a whole database eagerly measured +42 MB [V]. |
| Fuzzy matching | frizbee 0.13.0 | Zero dependencies; identical to blink.cmp, so ranking feels like Neovim's blink.cmp [V]. |
| Diff | similar 3.2.0 only: Myers line diff with **no deadline** (deterministic hunks; changed in the M1 review), and `iter_inline_changes` with an 8 ms deadline for changed words | 4.8 ms for 281 hunks on 7k lines; typical agent edits take well under 1 ms [V]. |
| Spell check | libspelling 0.5.0 (system 0.4.10) + enchant/hunspell en_US. Words from the `spell.word_lists` files are added with `Checker::ignore_word` [V]. | GTK4-native. |
| Config | TOML: `~/.config/lemma/{config.toml,snippets.toml}` | Literal strings keep backslashes. |
| Chat rendering | pulldown-cmark (no default features) converted to Pango markup in selectable `gtk::Label`s | No webview. |
| Logging | `tracing` (already a dependency of ACP) + `tracing-subscriber` (env-filter, `LEMMA_LOG`) | |
| License | GPL-3.0-or-later | Required by the cwl data; compatible with poppler (GPL-2+). |


The adapter is installed by an in-app action, with no sudo:
```
npm i --prefix ~/.local/share/lemma/acp @agentclientprotocol/claude-agent-acp@0.89.1 --omit=optional --no-audit --no-fund
```
The app then launches `node <prefix>/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js`.

## 2. Workspace layout

| Crate | Kind | Responsibility |
|---|---|---|
| `lemma-core` | pure | `TextEdit`, `Origin`, `LineIndex`, `LatestSlot`, `Diagnostic`, `ProjectRoot` (path canonicalization), `fnv1a64` |
| `lemma-syntax` | pure | Scanner (`LineStates`, `Token`, `Context`); structure pass (`index_file`, `FileIndex`, `ProjectIndex`); `scan_bib`, `parse_aux`, `parse_magic` |
| `lemma-snippets` | pure | Snippet TOML, body → `Chunk`s, LuaSnip trigger semantics, 91 default snippets |
| `lemma-complete` | pure | L1/L2/L3 data, `Completer` (frizbee + bonuses), `WordList` |
| `lemma-review` | pure | `FileReview` (baseline, hunks, edit classification, accept/reject/undo, persistence), `str_replace`, `minimal_edits`, `unified_diff` |
| `lemma-build` | std + libc | Root and engine detection, latexmk runner, `BuildQueue`, `.log`/`.blg` parsers |
| `lemma-synctex` | FFI | libsynctex `Scanner` |
| `lemma-pdf` | glib/cairo/poppler, no GTK | PDF thread (render + sync), `PageLayout`, `ByteLru` |
| `lemma-agent` | no GTK | ACP thread (all ACP code in `acp.rs`), MCP server (`mcp.rs`), message enums |
| `lemma-mcp` | bin, std only | stdio ↔ unix-socket bridge |
| `lemma-config` | pure | Config schema and defaults |
| `lemma-app` | bin `lemma` | All GTK code (§10.5) |
| `tools/lemma-fake-agent` | bin | Scripted ACP agent plus MCP client for tests (M2) |
| `xtask` | bin | Data generation (cwl, unicode-math) |

Data and test directories:
- `data/snippets.default.toml`
- `data/core-extra.toml`: about 70 symbols missing under pdflatex names, such as `\square` and `\lozenge`
- `data/cwl/` (M4)
- `tests/fixtures/`, copied from the research corpus in M0

## 3. Threading and async model

| Thread | Lifetime | Owns | Communication |
|---|---|---|---|
| GTK main | always | All widgets and buffers, `Document`s, `FileReview`s, `LineStates`, `ProjectIndex`, completion and snippet matching | One `glib::spawn_future_local` receive loop per `async_channel` |
| `pdf` | from the first PDF | poppler `Document` (!Send), synctex `Scanner`, render queue | `PdfCmd` over `std::mpsc` (Want = latest wins) in; `PdfEvent` over async_channel out |
| `synctex` | one per SyncTeX parse (§11, PDF view) | `Scanner::open`, after freeing the scanner it replaces | The scanner back on the `pdf` thread's channel |
| build | one per build | latexmk (`process_group(0)`), blocking `wait`, log parsing | `BuildOutcome` over async_channel |
| jobs | on demand | `gio::spawn_blocking`: `index_file`, `scan_bib`, cwl decode, turn snapshots, math-preview TeX runs | Awaited handle |
| `agent` | from the first panel open until Stop or quit | `block_on(Client.builder()…connect_with(AcpAgent))` | `AgentCmd` in; `AgentEvent` out |
| `mcp` | while the agent runs | Blocking `UnixListener`; one thread per connection, one per in-flight `tools/call` | `ToolCall` → main; reply on a `SyncSender(1)` with `recv_timeout` |

Rules:
1. Main-thread work per keystroke must be ≤ 2 ms at ≤ 10k lines.
2. The main thread never calls poppler, never waits on a process, and never reads files synchronously. It uses `FileLoader`/`FileSaver` and gio async.
3. Structure indexing is sent to workers as an `Arc<str>` copy 150 ms after typing stops, never on every keystroke. Each result carries a version and stale results are dropped.
4. ACP handlers never await the user. Permission responders go into a `Mutex<HashMap<u64, Responder>>` and are answered from the command loop. `session/prompt` runs in `cx.spawn(...)` so the command loop stays live [V].
5. MCP threads block on `recv_timeout`; the main thread replies when the work is done. For `compile` it replies only when the build finishes.
6. Every worker body runs inside `catch_unwind`, and the worker is restarted with its last state if it panics.

## 4. Agent edit review

### 4.1 Data model
- **The rule.** Buffer and disk always hold the proposed text, and `hunks = diff(baseline, current)`.
  - A `FileReview` is created on the first agent edit to a file. Its baseline is the file's text just before that edit.
  - Later agent edits, in the same or later turns, keep that baseline, so hunks from several turns merge into one diff.
  - The review is dropped when `is_clean()` returns true.
- **Hunk fields:**
  - `base_lines`/`base_bytes` and `cur_lines`/`cur_bytes`;
  - `old_text`, drawn as phantom red lines;
  - `words`, the intra-line spans;
  - `user_touched`.
- **Hunk IDs.** `HunkId = fnv1a64(old_text, new_text, occurrence)`. IDs are stable across recomputes and across accepting or rejecting other hunks, even when those change the line count.
- **Origin.** Every buffer change carries an `Origin` from a `Cell` on the `Document`:
  - `User` is the default.
  - `Agent{call}`, `Review` and `Reload` are set by our own code around the edit.
  - `History` is set by the `undo`/`redo` before-handler and reset by an after-handler, `connect_closure("undo", true, …)`.
- **Signal wiring:**
  - `insert-text`/`delete-range` **before-handlers** build a `TextEdit` with pre-edit byte offsets (`LineIndex::line_start(iter.line()) + iter.line_index()`), call `review.on_edit(&e, origin)`, then `line_index.apply(&e)`.
  - **After-handlers** call `LineStates::apply_edit`, re-tag the changed lines, and schedule `recompute`: immediately for Agent and Review edits, 50 ms after the last User edit.

### 4.2 How agent writes arrive
1. **Prompt start:**
   - Save dirty project buffers (`agent.save_before_prompt = true`).
   - On the jobs thread, snapshot every project-graph file that is not open (`.tex`, `.bib`, local `.sty`), capped at 4 MiB in total.
   - Send `session/prompt`. Its blocks are:
     - the drained `<review_feedback>` text;
     - the user's text;
     - an embedded resource with the current file, cursor line and selection.
2. **Claude calls an MCP tool** such as `mcp__lemma__edit{file_path, old_string, new_string, replace_all}`. The bridge passes it through the socket to the MCP thread, which builds a `ToolCall{call, req, reply}` and sends it to the main thread. `agent_edits::apply` then:
   1. resolves the path with `ProjectRoot::resolve`, refusing anything outside the root;
   2. opens or loads the `Document`. A file that is not open is loaded into a `Closed{shadow: String}` entry; no GTK buffer is created;
   3. runs `str_replace` against the **live** text. The errors are:
      - `"old_string not found — the user may have edited this region; call read_file"`;
      - `"old_string matches N times; add context or set replace_all"`;
   4. creates the `FileReview` if the file has none;
   5. applies the minimal edits in one user action with `Origin::Agent{call}`, so each tool call is one undo step. The user's cursor, marks and scroll position are untouched;
   6. calls `recompute`;
   7. **awaits the save**: `FileSaver` for open files (atomic replace, keeps newline type and encoding), `replace_contents_future` for closed ones;
   8. replies with `"Edited <rel>: lines a–b"` plus the region ±3 lines in `cat -n` format, plus any drained `ReviewEvent`s as `<review_feedback>…</review_feedback>`.
3. **`write`:** on an existing file it becomes `minimal_edits`. A new file gets an empty baseline and is marked `new_file`.
4. **Edits that bypass our tools** (Bash `sed -i`, subagents) are caught by a `gio::FileMonitor` on each project directory. Our own writes are recognised by content hash. A change during a turn, or within 10 s after it:
   - open file: applied as `minimal_edits(buffer, disk)` with `Origin::Agent{call: 0}`;
   - closed file: gets a review with the turn snapshot as baseline;
   - file not in the snapshot: baseline from `git show HEAD:<path>` if tracked; otherwise a toast "changed outside review" and the file is reloaded.

   Outside a turn, a clean buffer reloads with `Origin::Reload`, and a dirty one shows an infobar (Reload / Keep mine).

### 4.3 Inline rendering
- **Added lines:** the shared tag `review-added` gives a green paragraph background (alpha 0.18). Changed words get `review-added-word` (alpha 0.35).
- **Anchor and reserved space.** Each hunk's anchor line gets an anchor tag with the highest priority. It sets `pixels-above-lines = header_h + deleted_h`:
  - `header_h` is about 0.9 of a line height;
  - `deleted_h` is the height of the cached `pango::Layout` of `old_text`, wrapped to the text width when soft-wrap is on.
  - Anchors that reserve the same space share one tag (one per value of `pixels-above-lines`, and one per value of `pixels-below-lines`), not one per hunk: see §11, "Review overlay tags".
- **Where the anchor goes:**
  - the first `cur` line; for a pure deletion, the line after it;
  - at the end of the file, when no following line exists, the last line, using `pixels-below-lines` and drawing below it;
  - line 1 works normally.
- **Painting.** `TexView::snapshot_layer(AboveText)` calls `review_overlay::paint`. For each visible hunk it paints an opaque base, then a red tint, then the layout.
  - Deleted words are drawn with a darker red `AttrList` background.
  - From M2 the layout also carries syntax colours from `lemma-syntax` tokens.
  - Layouts are cached by content (the deleted text and words, the width, the scheme and the syntax context), not by `HunkId`: typing inside a hunk does not lay its deleted text out again.
- **Action bars.** A pool of 8 `ReviewBar` overlays (`Accept ⌥Y · Reject ⌥N · 3/7`) is placed with `add_overlay`/`move_overlay` in the header band of hunks near `visible_rect`. Positions are recomputed in an idle callback after the vertical adjustment's `changed` or `value-changed`, after a recompute, and after a resize. **Buttons never cover text.**
- **`ReviewGutter`** (subclass of `GutterRenderer`):
  - draws line numbers aligned to the text baseline (`iter_location().y` → `buffer_to_window_coords(Left)`); stock line numbers are turned off;
  - draws a green bar on added lines, a red wedge at each anchor, and amber when `user_touched`.
  Diagnostics use the stock marks renderer.
- **Other review UI:**
  - A thin accent outline marks the hunk under the cursor.
  - An `adw::Banner` reads "Claude changed 3 files · 7 hunks" with a "Review" button.
  - The **Changes** sidebar lists every touched file (open or not) with counts, Accept all / Reject all per file, and a "Plain diff" toggle that shows `unified_diff` in a read-only TextView for screen readers.
  - Alt+D (reading mode) shrinks each band to the header and hides the phantom text.

| Action | Chord (any mode) | Vim ex command |
|---|---|---|
| Next / previous hunk (across files, opening them) | Alt+] / Alt+[ | `:Hnext` / `:Hprev` |
| Accept / reject the hunk at the cursor | Alt+Y / Alt+N | `:Accept` / `:Reject` |
| All hunks in this file | Alt+Shift+Y / Alt+Shift+N | `:AcceptAll` / `:RejectAll` |
| All files | Ctrl+Alt+Y / Ctrl+Alt+N | `:AgentAcceptAll` / `:AgentRejectAll` |
| Revert the last accept or reject | Alt+U | `:Hundo` |
| Reading mode | Alt+D | — |

- After a decision the cursor moves to the next hunk and the view centres it.
- Ex commands go through `connect_execute_command`; it must return `true` when the command is handled.
- Normal-mode `]h`/`[h` depend on our capture-phase key layer running before VimIMContext. **Verified in M2** with real key events: the layer sits on the editor's ScrolledWindow, whose capture phase always runs first. The chords always work.

### 4.4 Accept, reject and user edits
- **Accept:** `baseline[base] := current[cur]`. No buffer change and no save. Journalled for Alt+U.
- **Reject:** returns a `TextEdit` that restores `old_text`. It is applied with `Origin::Review` as one undoable action, then saved, and a `Rejected` event is queued. If the hunk was `user_touched`, a toast says "Rejected — your edits in this hunk were discarded · Undo".
- **A new file** whose only hunk is rejected is moved to the trash with `gio::File::trash` and dropped. **A deleted file** can be restored from the snapshot.
- **User or History edits outside every hunk** (exact rule in §10.3) are applied to the baseline at the mapped offset, so the user's own typing never becomes a hunk.
- **User or History edits inside a hunk** become part of the proposal and set `user_touched`, and one `UserEditedInHunk` event is sent per hunk per turn. Accept then keeps the user's version.
- **Undo:**
  - Ctrl+Z or Vim `u` is linear over the buffer. Undoing an agent step shrinks or removes its hunk, and an `UndoneByUser` event is sent.
  - **Undoing a Reject:** a History edit inside the restored range of the most recent unconsumed Reject is classified as Agent, so the hunk re-opens with the **same HunkId** instead of being folded into the baseline.
  - Accept is not in the buffer's undo history; Alt+U reverts it from the journal (last 100 actions).

### 4.5 Saving, persistence and what the agent sees
- **Before acceptance, disk already holds the agent's version.** latexmk, the agent's built-in Read and Grep, and git all see the same state.
  - Reject writes the restored text.
  - Accept writes nothing.
  - An agent edit to an open buffer also saves any unsaved user typing in that buffer. This is documented behaviour.
- **Persistence.** Reviews are saved as `Vec<PersistedReview>{path, baseline, current_hash}` to `$XDG_STATE_HOME/lemma/review/<fnv(root)>.json`, debounced 500 ms, through gio. On reopen, a review is restored only if `fnv1a64(disk) == current_hash`, and a banner announces it. There is no blocking dialog on quit.
- **What the agent sees:**
  - `read_file` returns the live buffer: pending hunks applied, rejected ones reverted, the user's unsaved text included, with no markers.
  - Feedback arrives on the next tool result, as `<review_feedback>` before the next prompt, and on demand through `get_review_state`.
- **Builds:** when a turn ends and any file changed during it, a build starts.

## 5. Editor features and typing helpers
- **Context engine** (`LineStates::context_at`, under 50 µs) follows VimTeX's rules:
  - math delimiters `$`, `$$`, `\(`, `\[`;
  - math environments (equation, align, alignat, flalign, gather, multline, (x)xalignat, eqnarray, math, displaymath, dmath, all starred too, plus a user list in config);
  - `\text`-like commands switch back to text, where `$` re-enters math; `\ensuremath` enters math;
  - verbatim environments, `\verb`, `\lstinline`, `\mintinline`;
  - comments; `\$` and `\%` are literal;
  - a blank line closes inline math;
  - `tikzcd` is **not** math.
- **Snippets.** LuaSnip-style snippets are ported into `data/snippets.default.toml` (planned: 91; the final curated set has 71): line-begin environment snippets, text and math autosnippets, and manual snippets. `~/.config/lemma/snippets.toml` overrides them and reloads live.
  - **Trigger rules:**
    - wordTrig: `xsr` does not fire; `x sr` and `)sr` do.
    - A `math` snippet requires a math context and a previous character other than `\`, so `\sum` never expands.
    - `line_begin`; priority.
  - **When autosnippets fire:** only on a single typed character, never on paste, agent edits or undo, and never in comments or verbatim.
  - **Undo:** each expansion is one undo step, so Ctrl+Z gives back the literal trigger.
  - **Tab** expands a manual snippet even while the popup is open; otherwise it jumps to the next stop, otherwise it steps out past the next closing delimiter. Shift+Tab jumps back.
- **Completion** (`CompletionProviderImpl`; triggers on `\`, `{`, and `,` inside ref/cite arguments):
  - **Sources by context:**
    - commands, filtered by math or text mode, with glyphs (`\Z` → ℤ from the user's macros);
    - environments after `\begin{`;
    - labels after the ref/eqref/cref family, showing environment, title and `.aux` number;
    - bib keys with author, year and title;
    - files and packages;
    - dictionary words (in text only, at least 2 characters, at most 12).
  - **Ranking:** frizbee score, +20 for snippets, +12 for user macros, +8 for names used in the project, +4 for L1, −8 for dictionary words, plus recency. An exact line-begin snippet trigger hides everything else (the user's blink rule). At most 50 proposal objects.
- **Pairing:**
  - `{}`, `[]`, `()`; in math also `\{\}` and `\left(` → `\right)`;
  - a smart `$` that types over the closing `$` and never pairs after `\`;
  - typing a closing character steps over it; Backspace on an empty pair deletes both;
  - typing `}` that completes `\begin{x}`, or pressing Enter after it, inserts `\end{x}` when a forward scan shows the environment is unclosed;
  - Enter in a list inserts `\item`.
  - Live renaming of the matching `\begin`/`\end` comes in M4.
- **Spell check:** a libspelling adapter is created after the first frame. The scanner applies `no-spell-check` to commands, math, verbatim, the arguments of label/ref/cite/begin/end/usepackage/input/includegraphics/bibliography, and comments. Text arguments such as `\emph` and `\caption` are checked.
- **Diagnostics:**
  - log errors and warnings become sourceview `Mark`s with `MarkAttributes`, a `Pango::Underline::Error` line tag, and an entry in a Problems list;
  - "Ask agent to fix" on each entry (M2);
  - end-of-line `Annotations` (v5_18) come in M3 **[U]**.
- **Navigation (M3):**
  - an outline panel with KOMA commands, theorem children and `.aux` numbers;
  - Ctrl+click on a reference goes to its `\label`, on a citation to its `.bib` entry, on an include to the file;
  - hovering a reference shows its number and source; hovering a citation shows the entry.
- **Math hover preview (M3):** after 300 ms in math, a popover shows the rendered formula.
  - **Pipeline:** the `mylatexformat` format, rebuilt on the jobs thread when the preamble's hash changes (0.29 s); then `pdflatex -fmt` on a standalone body (90–115 ms [V]); then poppler.
  - **Cache:** 64 entries.
  - **No warm TeX process**, to save RAM.
  - **Fallback:** a full standalone build (about 230 ms) when the format cannot be built (fontspec documents **[U]**).
- **Vim:** VimIMContext is off by default since M5 (`[editor] vim`); the menu, Preferences and `win.toggle-vim` turn it on.
  - `:w` saves and builds through `connect_write` **[U: interactive only]**.
  - VimTeX operators and text objects come in M4, after the key-layer check.

## 6. Build and preview pipeline
- **Triggers:**
  - saving (Ctrl+S or `:w`);
  - F5;
  - the agent's `compile` tool;
  - the end of an agent turn in which files changed.

  Dirty project buffers are saved first, because latexmk decides by content hashes.
- **Queue:** `BuildQueue` allows one running build and one waiting; a newer request replaces the waiting one. A running build is never killed by a new request. Stop, or a 180 s timeout, calls `killpg(SIGTERM)` and then SIGKILL after 500 ms. `killpg` is required: SIGTERM to latexmk alone left pdflatex orphaned [V].
- **Invocation**, run in the root document's directory with `process_group(0)` and stdin from /dev/null, after creating `.build/<dir>` for each `\include`d directory:
  ```
  max_print_line=10000 error_line=254 half_error_line=238 \
  latexmk [-pdf|-lualatex|-xelatex|<none>] -synctex=1 -interaction=nonstopmode -file-line-error -recorder -outdir=.build ./<root>.tex   # ./ so a leading '-' in a file name is never an option
  ```
- **Engine:** a `% !TEX program` comment wins (latexmk ignores it [V], so we pass the flag). Otherwise, if a latexmkrc exists, pass no flag. Otherwise lualatex when fontspec or unicode-math is loaded, else pdflatex.
- **Root:** `% !TEX root`, then the subfiles parent, then the file itself if it has `\documentclass` and `\begin{document}`, then the reverse include graph, then `.fls`, then the remembered choice, otherwise ask once and store the answer in `~/.local/state/lemma/projects.toml`.
- **After the build:**
  - exit code 0 or 12; the log and `.blg` files are parsed on the build thread with the research regexes;
  - "File not found" takes its line from the following "Emergency stop";
  - biber's temp-file paths are mapped back to the `.bib`;
  - runaway arguments are located with the search heuristic;
  - whenever the log has `Output written`, including on exit 12, the PDF is reopened with a new generation.
- **PdfView** is one custom widget implementing `gtk::Scrollable`; there is no widget per page.
  - **Layout:** continuous scrolling, fit-width by default.
  - **Zoom:** Ctrl+wheel or `GestureZoom`, anchored at the pointer. Existing textures are stretched immediately and re-rendered after 120 ms idle at `scale_q = round(zoom × surface.scale() × 8)`. `Surface::scale()` (v4_12) exists [S]. The user's eDP-1 runs at scale 1; fractional scales are **[U]**.
  - **Tiles:** pages larger than 16 MB are rendered in tiles of at most 4 MiB: full-width bands, or squares of about 1024² on a page over 4096 px wide (§11, PDF view, 2026-10-25).
  - **Render queue:** visible pages from the centre outward, then ±1 page. Older generations are dropped.
  - **Textures:** `glib::Bytes::from_owned` → `MemoryTexture` (B8g8r8a8Premultiplied), with no copy [V].
  - **Cache:** a `ByteLru` capped at 48 MiB.
  - **Reload:** old textures stay on screen until new ones arrive; the `Anchor{page, frac, x}` is preserved and clamped if the page count changes.
  - **Dark mode:** invert(1) followed by hue-rotate(180°), mapped into a soft paper..ink grey range (`lemma_pdf::dark`). The PDF thread applies it to the bitmaps of dark requests (`PageKey::dark`), so the view draws them as plain textures. A `Snapshot::push_color_matrix` over the view would be free on the GPU renderers, but GTK's cairo renderer (the default) runs it per pixel on the CPU on every frame: 12 ms for a 384×956 pane and 30 ms for 940×1000 [V], so scrolling missed frames. After a toggle, the textures of the other mode stand in through that colour matrix (or its inverse) until the pages are re-rendered, so the switch shows at once. It follows `adw::StyleManager`; Ctrl+Shift+I toggles it per document.
- **SyncTeX:**
  - **Forward** (Ctrl+Alt+J, optional follow-cursor debounced 200 ms) draws `(h, v−H, W, H)` in bp scaled by zoom × scale, scrolled to one third of the view and faded over 600 ms.
  - **Inverse** (Ctrl+click on the PDF) calls `to_page`, then `Scanner::inverse`, then opens the file at that line. Sync is per line only, because pdfTeX records column −1.
  - The scanner opens on the first sync and is reopened when the `.synctex.gz` modification time changes, on a helper thread of the PDF thread, so pages render meanwhile (§11, PDF view, 2026-10-25).
- **Link hover (M3)** works like sioyek: `link_mapping` gives the destination, and a popover renders that region of the target page.

## 7. Memory and performance budget

| Metric | Target | Basis |
|---|---|---|
| Start to first frame | ≤ 150 ms | `activate` reached at 53 ms [V] |
| Idle: 1k-line document, spell check on, no PDF | ≤ 100 MB RSS / ≤ 50 MB PSS | svprobe 98.5/45 [V]; libspelling adds about 10 MB [V] |
| Plus a 50-page PDF, cache full | ≤ 170 MB RSS / ≤ 110 MB PSS | gtkprobe 110 MB with 3 pages [V] + 48 MiB cache |
| 60k-line file (2.2 MB) | ≤ 150 MB RSS | Whole-buffer probe 144 MB [V]; scanner states 4 B/line; tags only on lines that have been on screen |
| Main-thread work per keystroke (≤ 10k lines) | ≤ 2 ms p99; keystroke to frame ≤ 8 ms p99 | Buffer insert 28 µs [V]; incremental rescan in µs |
| Hunk recompute | ≤ 8 ms on 7k lines; < 1 ms for typical edits (no deadline: pathological whole-file rewrites of 60k lines take 0.5–0.7 s) | similar Myers [V] |
| Completion | ≤ 3 ms | Static tables, frizbee |
| Latexmk exit to first page on screen | ≤ 30 ms | 4–11 ms per page [V] |
| SyncTeX query | ≤ 5 ms | About 50 µs [V] |
| Agent inside the editor process | ≤ 3 MB | Threads and channels only |
| Agent outside the editor | About 360 MB while running (node 123 MB + claude about 240 MB [V]) | Started when the panel first opens; Stop button; stopped after `agent.idle_stop_min` idle minutes (§11) |

**Techniques:**
- System GTK, linked dynamically. No tokio, webview, JS engine or bundled C libraries.
- Lazy loading: spell check after the first frame, cwl per package, SyncTeX on the first sync (after a rebuild, parsed ahead only when SyncTeX was used since the build before or in the last five minutes), the preview format on the first hover, the agent when its panel opens.
- `mallopt(M_MMAP_THRESHOLD, 1 MiB)` and `mallopt(M_ARENA_MAX, 2)` at startup. When the PDF pane is hidden, or the window has been inactive for 60 s (unless the PDF is in a window of its own that is active), shrink the cache to the visible pages, free the SyncTeX scanner (`PdfCmd::Trim`, about 85 KiB per page) and call `malloc_trim(0)`, on the PDF thread too after the scanner is freed. (`lemma-app/src/heap.rs` and `lemma-pdf/src/heap.rs`: glibc only; on macOS `heap::trim` is `malloc_zone_pressure_relief`, see §12.)
- Keep GTK's default Vulkan renderer: `GSK_RENDERER=gl` measured about +75 MB [V]. `cairo` measured −30 MB and is documented as a low-memory option. M5: `[editor] renderer = "auto"|"cairo"|"vulkan"|"gl"` sets `GSK_RENDERER` at start-up unless the environment has it; `auto` (the default) is cairo: measured on the user's machine it saves about 27 MB and 140 ms to the first frame, types as fast, and still scrolls the PDF at 60 fps (docs/STATUS.md, "M5 performance").
- Closed documents are kept as `String`s; baselines exist only while hunks exist.
- **Measurement:**
  - `lemma --mem-report` reads `/proc/self/smaps_rollup` **before** any snapshot, because GSK `render_texture` adds about 60 MB.
  - `scripts/rss.sh` compares against `scripts/budgets.toml` and fails at more than 15% over budget.

## 8. Milestones

Each milestone ends in a runnable app.

| Milestone | Runnable result | Work packages |
|---|---|---|
| **M0 Skeleton** | `lemma file.tex` opens, edits and saves with built-in latex highlighting, a Vim toggle, `--snapshot` and `--mem-report`. Contracts frozen. | WP0.1–0.3 (§10.7) |
| **M1 Review + write/build/view** | Saving builds; continuous PDF with diagnostics and SyncTeX both ways; `--apply-as-agent edits.json` shows reviewable hunks with full accept/reject/undo/navigation. Scanner, snippets and structure crates done and tested. | WP1.1–1.9 (§10.7), all parallel after M0 |
| **M2 Agent + typing** | Claude over ACP edits through MCP tools; multi-file hunks, Changes panel, permission cards (y/Y/n), persistence, watcher, turn snapshots. Scanner highlighting (language None), L1+L3 completion, snippets and autosnippets, pairing, `\end` auto-close, spell check. Ends with **one paid prompt, only after the user approves it**, to check that edits go through `mcp__lemma__edit` and whether `render_page` images reach the model. | 2.1 ACP thread (`acp.rs`); 2.2 MCP server and bridge; 2.3 `lemma-fake-agent`; 2.4 agent bridge and tools (main-thread handlers, turn snapshot, watcher); 2.5 agent panel; 2.6 Changes panel and persistence; 2.7 highlighting switch; 2.8 L1 data (xtask) + `Completer`; 2.9 completion provider, snippet driver, typing helpers, spell; 2.10 key layer + Vim ex commands (verify `]h` before VimIMContext) |
| **M3 Navigate & preview** | Outline, goto and hover for refs/cites, math hover preview, link hover, PDF search, word count, Annotations | — |
| **M4 Typing depth** | L2 cwl (fetch TeXstudio `completion/`), quick fixes, symbol palette, Unicode → LaTeX (pdflatex documents only), live `\begin`/`\end` rename, VimTeX `cse`/`dse`/`tsf`/`ie`/`ae`/`i$`, list and align helpers | — |
| **M5 Ecosystem & hardening** | Messages-API backend (API key; streaming design **[U]**), Codex/Gemini ACP presets with `fs/*` routing, idle stop + `session/load` (§11; tested against the fake agent), `lemma-mcp` for Claude Code in a terminal, memory/performance CI gates, PKGBUILD | — |

## 9. Top risks and mitigations
1. **ACP SDK and adapter churn.** Three breaking major versions in 3.5 months; the adapter ships almost daily.
   - Pin `=3.3.0` and adapter 0.89.1.
   - Keep all ACP code in `lemma-agent/src/acp.rs`.
   - Run fake-agent conformance tests, and upgrade only on purpose.
2. ~~**Edit routing has not been seen with a real model.**~~ **Verified 2026-10-11** (CLI 2.1.296, adapter 0.89.1). Claude used `ToolSearch` → `mcp__lemma__read_file` → `mcp__lemma__edit` → `mcp__lemma__compile` → `mcp__lemma__render_page`, with no built-in Edit/Write/Bash and no permission requests. The watcher plus turn snapshot remain the safety net.
3. **Vim fidelity** for a Neovim/VimTeX user. VimIMContext has no macro recording [S], and it is unknown whether the key layer can run before it **[U]**. The fallback for after v1 is `nvim --embed`.
4. **Inline diff polish.** Bar timing and soft-wrap heights. The prototype exists [V]; WP1.2 has snapshot and behaviour tests.
5. **Agent memory** of about 360 MB. It is external and started only when the panel opens. Stop button; the idle stop after `agent.idle_stop_min` minutes, which continues the conversation with `session/load` (§11); the API-key backend costs a few MB.
6. **Subscription policy.** Claude Code login reuse is for personal use; distributed builds should default to the API-key backend. The UI never offers the `bypassPermissions` or `auto` modes.
7. **CLI version and PATH.**
   - Store absolute paths to `node` and `claude` (mise installs them under `~/.local/share/mise/installs/...`, which may not be on the PATH of a desktop launch).
   - Check versions at agent start; ask the user to run `claude update`.
8. **The scanner may misread TeX.**
   - Property test: incremental scan equals full scan.
   - VimTeX rule tables.
   - A local corpus via `LEMMA_PERF_CORPUS`.
9. **Snippet regressions.** Every default snippet is exercised through real GtkSourceView chunk expansion in a GTK test (WP1.8), not just parsed.
10. **License:** GPL-3.0-or-later, because of the cwl data.

---

## 10. IMPLEMENTATION CONTRACT

### 10.1 Conventions
- **Repository:** `~/Projects/lemma`. Rust 1.98.1 toolchain; MSRV 1.92 (gtk4).
- **Positions:**
  - internal text positions are **UTF-8 byte offsets**;
  - lines are 0-based internally and 1-based in the UI, diagnostics and MCP;
  - GTK conversion: `offset = LineIndex::line_start(iter.line()) + iter.line_index()`.
- **Threads:** only the main thread touches GObjects, with one exception: poppler and cairo objects owned by the PDF thread or a job. Workers return plain data over `async_channel`.
- **Errors:** library error types are enums; there is no anyhow.
- **GTK tests:** integration tests with `harness = false` (`tests/gtk_*.rs` with their own `main`), so `gtk::init()` runs on one thread. If init fails they print `SKIP: no display` and exit 0. `WAYLAND_DISPLAY=wayland-1` is available.
- **Every WP must pass:**
  ```
  cargo fmt --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```
- **Performance tests:** `#[ignore]`d and named `perf_*`, run with `cargo test --release -- --ignored perf`.
- **Golden images:** mean absolute difference < 2/255; `UPDATE_GOLDEN=1` rewrites them.
- **Paths:**

  | Purpose | Path |
  |---|---|
  | Config | `~/.config/lemma/` |
  | State | `~/.local/state/lemma/{review/,projects.toml}` |
  | Data | `~/.local/share/lemma/acp/` |
  | Runtime | `$XDG_RUNTIME_DIR/lemma/mcp-<pid>.sock` |
  | Cache | `~/.cache/lemma/preview/` |
  | Inside the project | only `.build/` |

### 10.2 Cargo files

These are verified to resolve and pass `cargo check --workspace --all-targets` [V].

```toml
# Cargo.toml (workspace)
[workspace]
resolver = "3"
members = ["crates/*", "tools/lemma-fake-agent", "xtask"]

[workspace.package]
version = "0.2.0"
edition = "2024"
rust-version = "1.92"
license = "GPL-3.0-or-later"
publish = false

[workspace.dependencies]
gtk          = { package = "gtk4",       version = "=0.11.5", features = ["gnome_50"] }
gdk          = { package = "gdk4",       version = "=0.11.5", features = ["v4_22"] }
adw          = { package = "libadwaita", version = "=0.9.2",  features = ["v1_9"] }
sourceview5  = { version = "=0.11.2", features = ["v5_18"] }
libspelling  = "=0.5.0"
poppler      = { package = "poppler-rs", version = "=0.26.0", features = ["v26_1"] }
cairo        = { package = "cairo-rs",   version = "=0.22.9", features = ["v1_18", "png"] }
glib         = { version = "=0.22.10", features = ["v2_88"] }
gio          = { version = "=0.22.10", features = ["v2_88"] }
pango        = { version = "=0.22.9",  features = ["v1_58"] }
agent-client-protocol        = { version = "=3.3.0", features = ["process"] }
agent-client-protocol-schema = "=1.11.0"
futures-executor = "0.3.34"
async-channel    = "2.5.0"
similar        = { version = "3.2.0", features = ["inline"] }
frizbee        = "0.13.0"
ruzstd         = "0.9.0"
regex          = "1.13.1"
memchr         = "2.8.3"
serde          = { version = "1.0.229", features = ["derive"] }
serde_json     = "1.0.152"
toml           = "1.1.8"
pulldown-cmark = { version = "0.13.4", default-features = false }
libc           = "0.2.190"
tracing        = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
pkg-config = "0.3.34"
zstd       = "0.14.2"
proptest   = "1.11.0"
insta      = "1.49.0"
tempfile   = "3.27.0"
lemma-core     = { path = "crates/lemma-core" }
lemma-syntax   = { path = "crates/lemma-syntax" }
lemma-snippets = { path = "crates/lemma-snippets" }
lemma-complete = { path = "crates/lemma-complete" }
lemma-review   = { path = "crates/lemma-review" }
lemma-build    = { path = "crates/lemma-build" }
lemma-synctex  = { path = "crates/lemma-synctex" }
lemma-pdf      = { path = "crates/lemma-pdf" }
lemma-agent    = { path = "crates/lemma-agent" }
lemma-config   = { path = "crates/lemma-config" }

[workspace.lints.rust]
unsafe_op_in_unsafe_fn = "deny"
[workspace.lints.clippy]
all = { level = "warn", priority = -1 }

[profile.release]
lto = "thin"
codegen-units = 1
strip = true          # keep panic=unwind: worker panics are caught and the worker restarted
[profile.dev.package."*"]
opt-level = 2
```

Every crate's `[package]` uses `version.workspace = true`, and the same for `edition`, `rust-version`, `license` and `publish`, plus `[lints] workspace = true`. All dependencies are written `{ workspace = true }`.

| Crate | `[dependencies]` | dev / build dependencies |
|---|---|---|
| lemma-core | serde | dev: proptest |
| lemma-syntax | lemma-core, memchr | dev: proptest, insta |
| lemma-snippets | lemma-syntax, serde, toml | — |
| lemma-complete | lemma-syntax, lemma-snippets, frizbee, ruzstd | — |
| lemma-review | lemma-core, similar, serde | dev: proptest, serde_json |
| lemma-build | lemma-core, lemma-syntax, regex, libc | dev: insta, tempfile |
| lemma-synctex (`links = "synctex"`, `build = "build.rs"`) | — | build: pkg-config |
| lemma-pdf | lemma-core, lemma-synctex, poppler, cairo, glib, async-channel | — |
| lemma-agent | lemma-core, agent-client-protocol, futures-executor, async-channel, serde, serde_json, tracing | — |
| lemma-mcp (bin) | none (std only) | — |
| lemma-config | serde, toml | — |
| lemma-app (`[[bin]] name="lemma"`) | all `lemma-*` except lemma-mcp and lemma-synctex; gtk, gdk, adw, sourceview5, libspelling, glib, gio, pango, cairo, async-channel, serde, serde_json, pulldown-cmark, libc, tracing, tracing-subscriber | — |
| tools/lemma-fake-agent | agent-client-protocol-schema, serde, serde_json | — |
| xtask | zstd, regex, serde, toml | — |

The synctex build script:
```rust
// crates/lemma-synctex/build.rs
fn main() { pkg_config::Config::new().atleast_version("2.0").probe("synctex").expect("libsynctex not found"); }
```
(Since the macOS port, §12: on macOS, or with `LEMMA_SYNCTEX_BUNDLED=1`, it compiles the parser in
`crates/lemma-synctex/vendor/synctex/` instead.)

### 10.3 Public API of the GUI-free crates (frozen in M0)

Bodies are `todo!()` in M0. Doc comments are part of the contract.

```rust
// ===== lemma-core =====
pub struct TextEdit { pub range: Range<usize>, pub text: String }        // Clone, Debug, Eq, Serialize, Deserialize
impl TextEdit { pub fn insert(at: usize, text: impl Into<String>) -> Self; pub fn delete(range: Range<usize>) -> Self;
                pub fn apply(&self, s: &mut String); pub fn delta(&self) -> isize; }
pub fn apply_all(s: &mut String, edits: &[TextEdit]);                   // edits in DESCENDING range.start order
pub enum Origin { User, History, Agent { call: u64 }, Review, Reload }  // Copy
pub struct LineIndex { /* Vec<usize> line starts; count = '\n' + 1 */ }
impl LineIndex { pub fn new(text: &str) -> Self; pub fn line_count(&self) -> u32; pub fn line_start(&self, line: u32) -> usize;
    pub fn offset(&self, line: u32, col_byte: u32) -> usize; pub fn position(&self, offset: usize) -> (u32, u32);
    pub fn line_of(&self, offset: usize) -> u32; pub fn apply(&mut self, e: &TextEdit);      // pre-edit offsets
    pub fn line_delta(&self, e: &TextEdit) -> (u32, u32, u32); }                              // (first, removed, added)
pub struct LatestSlot<T>;  impl<T> LatestSlot<T> { pub fn new() -> Arc<Self>; pub fn put(&self, v: T);
    pub fn take_blocking(&self) -> Option<T>; pub fn try_take(&self) -> Option<T>; pub fn close(&self); }
pub enum Severity { Error, Warning, BadBox, Info }   pub enum DiagSource { Tex, Biber, Bibtex, Index }
pub struct Diagnostic { pub path: PathBuf, pub line: Option<u32> /*1-based*/, pub severity: Severity,
                        pub message: String, pub context: Option<String>, pub source: DiagSource }
pub enum PathError { OutsideProject(PathBuf), Io(String) }
pub struct ProjectRoot(PathBuf);
impl ProjectRoot { pub fn new(dir: &Path) -> io::Result<Self>; pub fn path(&self) -> &Path;
    /// Relative paths are joined to the root; canonicalize (the parent if the file does not exist yet); refuse `..`
    /// or symlink escapes; Err(OutsideProject) unless under the root.
    pub fn resolve(&self, p: &Path) -> Result<PathBuf, PathError>; pub fn relative<'a>(&self, p: &'a Path) -> Option<&'a Path>; }
pub fn fnv1a64(parts: &[&[u8]]) -> u64;   // FNV-1a 64; stable forever

// ===== lemma-syntax =====
pub trait Lines { fn line_count(&self) -> u32; fn line(&self, i: u32) -> Cow<'_, str>; }   // without '\n'
pub struct StrLines<'a> { pub text: &'a str, pub index: &'a LineIndex }                    // impl Lines
pub struct ScanConfig { pub math_envs: Vec<String>, pub verbatim_envs: Vec<String>, pub text_in_math_cmds: Vec<String> } // Default = VimTeX rules
pub enum TokenKind { Text, Command, Sectioning, Begin, End, EnvName, Brace, Bracket, Special, MathDelim, Math, MathCommand,
                     Comment, Verbatim, LabelArg, RefArg, CiteArg, FileArg, PackageArg, KeyArg }
impl TokenKind { pub fn no_spell(self) -> bool; }
pub struct Token { pub start: u32, pub end: u32, pub kind: TokenKind }                     // bytes within the line
pub enum ArgKind { Ref, Cite, Label, File, Env, Package, Key, Other }
pub enum Context { Text, Math { display: bool, env: Option<Box<str>> }, TextInMath, Comment, Verbatim,
                   CommandName { start: u32 }, Arg { cmd: Box<str>, idx: u8, kind: ArgKind, start: u32 } }
impl Context { pub fn is_math(&self) -> bool; }
pub struct LineStates;   // ~4 B per line (interned states)
impl LineStates { pub fn new(cfg: Arc<ScanConfig>) -> Self; pub fn rebuild(&mut self, src: &impl Lines);
    /// [first, first+removed) were replaced by `added` lines (already in src); rescan until the state converges;
    /// returns the lines to re-tag.
    pub fn apply_edit(&mut self, src: &impl Lines, first: u32, removed: u32, added: u32) -> Range<u32>;
    pub fn tokens(&self, line_no: u32, line: &str, out: &mut Vec<Token>);
    pub fn context_at(&self, line_no: u32, line: &str, byte: u32) -> Context; }            // < 50 µs
pub struct Magic { pub root: Option<PathBuf>, pub program: Option<String>, pub subfiles_parent: Option<PathBuf> }
pub struct OutlineItem { pub level: u8 /*0 part … 6 subparagraph, 7 theorem-like*/, pub title: String, pub line: u32, pub starred: bool, pub env: Option<String> }
pub struct Label { pub name: String, pub line: u32, pub env: Option<String>, pub title: Option<String> }
pub struct Reference { pub cmd: String, pub names: Vec<String>, pub line: u32 }
pub struct Citation { pub cmd: String, pub keys: Vec<String>, pub line: u32 }
pub struct EnvPair { pub name: String, pub begin_line: u32, pub end_line: Option<u32> }
pub enum IncludeKind { Input, Include, Subfile, Import, Graphics, Bibliography, Package }
pub struct Include { pub kind: IncludeKind, pub target: String, pub line: u32 }
pub struct MacroDef { pub name: String, pub nargs: u8, pub body: Option<String>, pub line: u32, pub is_env: bool, pub is_theorem: bool, pub is_math_operator: bool }
pub struct FileIndex { pub outline: Vec<OutlineItem>, pub labels: Vec<Label>, pub refs: Vec<Reference>, pub cites: Vec<Citation>,
    pub envs: Vec<EnvPair>, pub includes: Vec<Include>, pub macros: Vec<MacroDef>, pub packages: Vec<String>,
    pub magic: Magic, pub has_documentclass: bool, pub has_begin_document: bool }
pub fn index_file(text: &str) -> FileIndex;          pub fn parse_magic(text: &str) -> Magic;
pub struct BibEntry { pub key: String, pub kind: String, pub author: Option<String>, pub year: Option<String>, pub title: Option<String>, pub line: u32 }
pub fn scan_bib(text: &str) -> Vec<BibEntry>;        // tolerant: resume at the next '@'
pub struct AuxInfo { pub labels: BTreeMap<String, (String, String)> /* number, page */ }
pub fn parse_aux(text: &str) -> AuxInfo;
pub struct ProjectIndex { pub files: BTreeMap<PathBuf, Arc<FileIndex>>, pub bibs: BTreeMap<PathBuf, Arc<Vec<BibEntry>>>, pub aux: AuxInfo }
impl ProjectIndex { pub fn update_file(&mut self, path: PathBuf, idx: Arc<FileIndex>);
    pub fn find_label(&self, name: &str) -> Option<(&Path, &Label)>; pub fn all_labels(&self) -> Vec<(&Path, &Label)>;
    pub fn all_bib(&self) -> Vec<&BibEntry>; pub fn packages(&self) -> Vec<String>;
    pub fn project_files(&self, root: &Path) -> Vec<PathBuf>; pub fn includers_of(&self, file: &Path) -> Vec<PathBuf>; }

// ===== lemma-snippets =====
pub enum Cond { Any, Math, Text }                    // serde lowercase, default Any
pub struct SnippetDef { pub trigger: String, pub body: String, pub auto: bool, pub context: Cond,
    pub word_trig: bool /*default true*/, pub line_begin: bool, pub priority: i32, pub description: Option<String> }
/// Body syntax: `$N`, `${N}`, `${N:default}` (N = 0..9) are tab stops; a repeated N is a mirror; `${$}` is a literal `$`;
/// any other `$` is literal; backslashes are literal.
pub enum Chunk { Text(String), Stop { n: u8, default: String }, Mirror(u8) }
pub enum SnippetError { Toml(String), Body { trigger: String, msg: String }, DefaultHasDollar { trigger: String } }
pub fn parse_body(body: &str) -> Result<Vec<Chunk>, String>;     // appends Stop{0} if there is no $0
pub fn with_indent(chunks: &[Chunk], indent: &str) -> Vec<Chunk>; // GtkSourceView does not indent continuation lines [V]
pub struct SnippetSet;  pub struct SnippetMatch<'a> { pub def: &'a SnippetDef, pub chunks: &'a [Chunk], pub trigger_len: usize }
impl SnippetSet { pub fn defaults() -> Self; pub fn from_toml(src: &str) -> Result<Self, SnippetError>;
    pub fn merged(defaults: Self, user: Self) -> Self; pub fn len(&self) -> usize;
    pub fn auto_match(&self, line_before: &str, ctx: &Context) -> Option<SnippetMatch<'_>>;
    pub fn manual(&self, prefix: &str, line_before: &str, ctx: &Context) -> Vec<SnippetMatch<'_>>;
    pub fn is_exact_line_begin_trigger(&self, prefix: &str, line_before: &str) -> bool; }
// TOML: [[snippet]]  trigger = "ff"  body = '\frac{$1}{$2}'  auto = true  context = "math"

// ===== lemma-complete =====
pub enum Mode { Any, MathOnly, TextOnly }
pub struct CmdEntry { pub name: &'static str, pub snippet: &'static str, pub mode: Mode, pub package: &'static str, pub glyph: Option<char> }
pub struct EnvEntry { pub name: &'static str, pub body: &'static str, pub math: bool, pub package: &'static str }
pub enum ItemKind { Command, Environment, Snippet, Label, Citation, File, Package, UserMacro, Word }
pub enum Insert { Text(String), Snippet(Vec<Chunk>) }
pub struct Item { pub label: String, pub insert: Insert, pub kind: ItemKind, pub detail: Option<String>, pub glyph: Option<char>, pub score: i32 }
pub struct CompletionDb;  impl CompletionDb { pub fn builtin() -> Self; pub fn load_packages(&mut self, names: &[String]); }
pub struct WordList;      impl WordList { pub fn from_text(texts: &[&str]) -> Self; pub fn spell_words(&self) -> Vec<String>; }
pub struct CompletionRequest<'a> { pub ctx: &'a Context, pub prefix: &'a str, pub line_before: &'a str }
pub struct Completer;
impl Completer { pub fn new() -> Self;
    pub fn complete(&mut self, db: &CompletionDb, snippets: &SnippetSet, idx: &ProjectIndex, words: &WordList,
                    req: &CompletionRequest, out: &mut Vec<Item>);          // ≤ 50 items, best first
    pub fn record_accepted(&mut self, label: &str); }

// ===== lemma-review =====
pub struct HunkId(pub u64);   // fnv1a64(old_text, new_text, occurrence among equal pairs)
pub enum Side { Old, New }    pub struct WordSpan { pub side: Side, pub line: u32, pub bytes: Range<u32> }
pub struct Hunk { pub id: HunkId, pub base_lines: Range<u32>, pub cur_lines: Range<u32>, pub cur_bytes: Range<usize>,
                  pub base_bytes: Range<usize>, pub old_text: String, pub words: Vec<WordSpan>, pub user_touched: bool }
pub enum EditClass { Outside, Inside(HunkId) }
pub enum ReviewEvent { Accepted { lines: Range<u32> }, Rejected { lines: Range<u32>, proposed: String, kept: String },
                       UserEditedInHunk { lines: Range<u32> }, UndoneByUser { lines: Range<u32> } }   // Serialize
pub enum ReviewError { NoSuchHunk(HunkId), Stale }
pub enum UndoAction { Unaccepted, Edits(Vec<TextEdit>) }
pub struct HunkDelta { pub added: Vec<HunkId>, pub removed: Vec<HunkId>, pub changed: Vec<HunkId> }
pub struct PersistedReview { pub path: PathBuf, pub baseline: String, pub current_hash: u64, pub created_unix: u64 }
pub struct FileReview;
impl FileReview { pub fn new(baseline: String) -> Self; pub fn baseline(&self) -> &str; pub fn hunks(&self) -> &[Hunk];
    pub fn hunk(&self, id: HunkId) -> Option<&Hunk>; pub fn hunk_at_line(&self, cur_line: u32) -> Option<&Hunk>;
    /// EVERY buffer change, before it applies, with pre-edit offsets [a,b). For cur_bytes [s,e):
    /// Inside(h) iff (a<b && a<e && b>s) || (a==b && s<=a && a<e) || (s==e && a<s && b>s).
    /// Outside User/History edits are applied to the baseline at x − (net growth of hunks ending at or before x).
    /// Inside User/History edits set user_touched (one UserEditedInHunk per hunk per turn).
    /// Agent/Review/Reload never touch the baseline. A History edit within the restored range of the most recent
    /// unconsumed Reject counts as Agent (undo-of-reject re-opens the same HunkId). Ranges are shifted until recompute.
    pub fn on_edit(&mut self, e: &TextEdit, origin: Origin) -> EditClass;
    pub fn recompute(&mut self, current: &str) -> HunkDelta;     // similar Myers, no line-diff deadline (deterministic); word spans with an 8 ms deadline
    pub fn accept(&mut self, id: HunkId, current: &str) -> Result<(), ReviewError>;
    pub fn reject(&mut self, id: HunkId, current: &str) -> Result<TextEdit, ReviewError>;   // caller applies with Origin::Review
    pub fn accept_all(&mut self, current: &str); pub fn reject_all(&mut self, current: &str) -> Vec<TextEdit>; // descending
    pub fn undo_review_action(&mut self, current: &str) -> Option<UndoAction>;              // journal of 100
    pub fn drain_events(&mut self) -> Vec<ReviewEvent>; pub fn is_clean(&self) -> bool;
    pub fn to_persisted(&self, path: PathBuf, current: &str) -> PersistedReview;
    pub fn from_persisted(p: &PersistedReview, disk: &str) -> Option<Self>; }              // only if hash(disk) matches
pub enum EditError { NotFound, NotUnique(usize), Empty }
pub fn str_replace(current: &str, old: &str, new: &str, all: bool) -> Result<Vec<TextEdit>, EditError>; // minimal, descending
pub fn minimal_edits(current: &str, new: &str) -> Vec<TextEdit>;
pub fn unified_diff(path: &str, old: &str, new: &str) -> String;

// ===== lemma-build =====
pub enum Engine { PdfLatex, LuaLatex, XeLatex, Latexmkrc }
pub enum RootSource { MagicComment, Subfiles, SelfDocument, IncludeGraph, Fls, Remembered, Unknown }
pub struct RootGuess { pub root: Option<PathBuf>, pub source: RootSource }
pub fn detect_root(file: &Path, text: &str, idx: &ProjectIndex, remembered: Option<&Path>) -> RootGuess;
pub fn detect_engine(root_text: &str, root_dir: &Path) -> Engine;
pub struct BuildRequest { pub root: PathBuf, pub engine: Engine, pub outdir: String, pub mirror_dirs: Vec<PathBuf>,
                          pub timeout: Duration, pub latexmk: PathBuf }
pub fn command_line(req: &BuildRequest) -> (Vec<OsString>, Vec<(String, String)>);   // exact argv/env of §6
pub struct Canceller;  impl Canceller { pub fn cancel(&self); }                        // killpg TERM, KILL after 500 ms
pub struct Build;      impl Build { pub fn start(req: &BuildRequest) -> io::Result<Build>; pub fn canceller(&self) -> Canceller;
                                    pub fn wait(self) -> BuildOutcome; }              // blocking
pub enum BuildStatus { Success, Errors, Cancelled, TimedOut, SpawnFailed(String) }
pub struct BuildOutcome { pub status: BuildStatus, pub exit_code: Option<i32>, pub elapsed: Duration, pub report: LogReport,
                          pub pdf: Option<PathBuf>, pub synctex: Option<PathBuf> }
pub struct LogReport { pub diagnostics: Vec<Diagnostic>, pub rerun: bool, pub output: Option<(PathBuf, u32)>, pub fatal: bool }
pub fn parse_log(log: &str, workdir: &Path, exists: &dyn Fn(&Path) -> bool) -> LogReport;
pub fn parse_blg(blg: &str, bib_sources: &[PathBuf]) -> Vec<Diagnostic>;
pub fn locate_runaway(runaway_text: &str, sources: &[(PathBuf, String)]) -> Option<(PathBuf, u32)>;
pub struct BuildQueue;  impl BuildQueue { pub fn request(&mut self, r: BuildRequest) -> Option<BuildRequest>;
                                          pub fn finished(&mut self) -> Option<BuildRequest>; pub fn is_running(&self) -> bool; }

// ===== lemma-synctex =====  (ffi module: the 13 C functions with exact signatures from /usr/include/synctex/synctex_parser.h;
//   synctex_status_t = c_long; floats are c_float)
pub struct SyncBox { pub page: u32 /*1-based*/, pub h: f32, pub v_top: f32, pub width: f32, pub height: f32 }  // bp, top-left origin
pub struct SyncTarget { pub file: PathBuf, pub line: u32 }
pub struct Scanner;   // unsafe impl Send; Drop calls synctex_scanner_free
impl Scanner { pub fn open(pdf: &Path) -> Option<Self>;
    /// v_top = box_visible_v − box_visible_height; height = box_visible_height + box_visible_depth; width = |box_visible_width|
    pub fn forward(&mut self, file: &Path, line: u32) -> Vec<SyncBox>;
    pub fn inverse(&mut self, page: u32, x_bp: f32, y_bp: f32) -> Option<SyncTarget>; }

// ===== lemma-pdf =====
pub struct PageKey { pub generation: u64, pub page: u32, pub scale_q: u16 /* round(zoom*scale*8) */, pub tile: Option<(u16, u16)>, pub dark: bool }
pub struct RenderReq { pub key: PageKey }
pub struct Link { pub area: (f64, f64, f64, f64), pub dest: LinkDest }  pub enum LinkDest { Page { page: u32, y_bp: Option<f64> }, Uri(String), Other }
pub struct Rendered { pub key: PageKey, pub width: i32, pub height: i32, pub stride: i32, pub data: cairo::ImageSurfaceDataOwned }
pub enum PdfEvent { Opened { generation: u64, page_sizes: Vec<(f64, f64)> }, OpenFailed { generation: u64, error: String },
    Rendered(Rendered), Forward { generation: u64, boxes: Vec<SyncBox> }, Inverse { target: Option<SyncTarget> },
    Links { generation: u64, page: u32, links: Vec<Link> } }
pub enum PdfCmd { Open { generation: u64, pdf: PathBuf }, Want(Vec<RenderReq>), Forward { file: PathBuf, line: u32 },
                  Inverse { page: u32, x_bp: f64, y_bp: f64 }, Links { page: u32 }, Shutdown }
pub fn spawn(events: async_channel::Sender<PdfEvent>) -> PdfHandle;   impl PdfHandle { pub fn send(&self, cmd: PdfCmd); }
pub struct Anchor { pub page: u32, pub frac: f64, pub x: f64 }
pub struct PageLayout;
impl PageLayout { pub fn new(sizes: Vec<(f64, f64)>, gap: f64) -> Self; pub fn page_count(&self) -> u32;
    pub fn content_size(&self, zoom: f64) -> (f64, f64); pub fn page_rect(&self, page: u32, zoom: f64, view_width: f64) -> (f64, f64, f64, f64);
    pub fn visible(&self, y0: f64, y1: f64, zoom: f64) -> Range<u32>;
    pub fn to_view(&self, page: u32, x_bp: f64, y_bp: f64, zoom: f64, view_width: f64) -> (f64, f64);
    pub fn to_page(&self, x: f64, y: f64, zoom: f64, view_width: f64) -> Option<(u32, f64, f64)>;
    pub fn anchor_at(&self, y: f64, x: f64, zoom: f64) -> Anchor; pub fn y_for_anchor(&self, a: Anchor, zoom: f64) -> f64;
    pub fn fit_width_zoom(&self, view_width: f64) -> f64; }
pub struct ByteLru<K, V>;
impl<K: Eq + Hash + Clone, V> ByteLru<K, V> { pub fn new(cap_bytes: usize) -> Self; pub fn insert(&mut self, k: K, v: V, bytes: usize) -> Vec<K>;
    pub fn get(&mut self, k: &K) -> Option<&V>; pub fn retain(&mut self, f: impl FnMut(&K) -> bool);
    pub fn shrink_to(&mut self, cap_bytes: usize); pub fn used(&self) -> usize; }

// ===== lemma-agent =====
pub struct AgentConfig { pub node: PathBuf, pub adapter_js: PathBuf, pub claude_exe: PathBuf, pub cwd: PathBuf, pub mcp_bridge: PathBuf,
                         pub mcp_socket: PathBuf, pub mcp_token: String, pub system_prompt_append: String, pub raw_log: Option<PathBuf> }
pub enum PromptBlock { Text(String), Resource { uri: String, mime: String, text: String } }
pub enum AgentCmd { Prompt(Vec<PromptBlock>), Cancel, Permission { request: u64, option_id: Option<String> },
                    SetMode(String), SetConfig { id: String, value: String }, Shutdown }
pub enum ToolStatus { Pending, InProgress, Completed, Failed }
pub enum StopReason { EndTurn, MaxTokens, MaxTurnRequests, Refusal, Cancelled }
pub struct ConfigOpt { pub id: String, pub name: String, pub current: String, pub values: Vec<(String, String)> }
pub struct PermOption { pub id: String, pub name: String, pub kind: String }
pub enum AgentEvent { Ready { session: String, agent: String, modes: Vec<(String, String)>, mode: String, options: Vec<ConfigOpt> },
    MessageChunk(String), ThoughtChunk(String), ToolCall { id: String, title: String, kind: String, status: ToolStatus },
    ToolUpdate { id: String, title: Option<String>, status: Option<ToolStatus> }, Plan(Vec<(String, String)>),
    Commands(Vec<(String, String)>), ModeChanged(String), ConfigChanged(Vec<ConfigOpt>),
    Permission { request: u64, title: String, options: Vec<PermOption> }, TurnEnded(StopReason), Error(String), Exited(Option<i32>) }
pub enum EditorRequest { ReadFile { path: PathBuf, offset: Option<u32>, limit: Option<u32> },
    Edit { path: PathBuf, old_string: String, new_string: String, replace_all: bool }, Write { path: PathBuf, content: String },
    ListFiles, Outline { path: Option<PathBuf> }, Compile, Diagnostics { path: Option<PathBuf> }, Selection, RenderPage { page: u32 }, ReviewState }
pub enum ToolContent { Text(String), Png(Vec<u8>) }    pub struct ToolReply { pub content: Vec<ToolContent>, pub is_error: bool }
pub struct ToolCall { pub call: u64, pub req: EditorRequest, pub reply: std::sync::mpsc::SyncSender<ToolReply> }  // exactly one reply
pub struct AgentHandle { pub cmd: async_channel::Sender<AgentCmd>, pub events: async_channel::Receiver<AgentEvent>,
                         pub tools: async_channel::Receiver<ToolCall> }
pub fn spawn(cfg: AgentConfig) -> io::Result<AgentHandle>;
pub mod mcp { pub const TOOL_TIMEOUT: Duration /*30 s*/; pub const PROTOCOL_VERSIONS: &[&str] /*["2025-11-25","2025-06-18"]*/;
    pub fn tool_definitions() -> Value; pub fn handle_basic(msg: &Value) -> Option<Value>;
    pub fn parse_tool_call(name: &str, args: &Value) -> Result<EditorRequest, String>;
    pub fn reply_to_json(r: &ToolReply) -> Value; pub fn base64(bytes: &[u8]) -> String;
    pub struct McpServer { pub socket: PathBuf }
    impl McpServer { pub fn start(socket: &Path, token: String, tools: async_channel::Sender<ToolCall>) -> io::Result<McpServer>; pub fn stop(&self); } }

// ===== lemma-config =====
pub struct Config { pub editor: EditorCfg, pub build: BuildCfg, pub pdf: PdfCfg, pub agent: AgentCfg, pub spell: SpellCfg, pub keys: KeysCfg }
// EditorCfg{vim=true, font="Monospace 11", scheme_light="Adwaita", scheme_dark="Adwaita-dark", math_envs=[], wrap=true}
// BuildCfg{on_save=true, on_agent_turn_end=true, outdir=".build", timeout_s=180, latexmk="latexmk"}
// PdfCfg{cache_mib=48, dark_invert=true(follows style), follow_cursor=false}
// AgentCfg{node, claude (absolute; auto-detected once), adapter_dir="~/.local/share/lemma/acp", adapter_version="0.89.1",
//          save_before_prompt=true, snapshot_cap_mib=4, idle_stop_min=10 (M5)}
// SpellCfg{enabled=true, language="en_US", word_lists=["~/.config/nvim/dictionaries/algebraic-geometry.txt","…/category-theory.txt"]}
pub fn parse(src: &str) -> Result<Config, String>;
pub struct Paths { pub config_dir: PathBuf, pub state_dir: PathBuf, pub data_dir: PathBuf, pub runtime_dir: PathBuf, pub cache_dir: PathBuf }
```

### 10.4 Agent wire contract
- **ACP `initialize`:** `ProtocolVersion::V1`, `ClientCapabilities::new().fs(FileSystemCapabilities::new()).terminal(false)` (fs routing waits for M5), `Implementation::new("lemma", VERSION)`.
- **ACP `session/new`:**
  - `cwd` = project root;
  - `mcp_servers = [McpServer::Stdio(McpServerStdio::new("lemma", <lemma-mcp>).env([LEMMA_MCP_SOCKET, LEMMA_MCP_TOKEN]))]`;
  - `meta`:
    ```json
    {"claudeCode":{"options":{"disallowedTools":["Edit","Write","NotebookEdit"],"allowedTools":["mcp__lemma__*"]}},
     "systemPrompt":{"append":"<APPEND>"}}
    ```
- **Agent process:** `AcpAgentConfig::new(node).arg(adapter_js).env("CLAUDE_CODE_EXECUTABLE", claude)` [V].
- **Modes offered:** `default` and `plan` only.
- **APPEND text:**
  > You are working inside Lemma, a LaTeX editor. The user reviews every change you make as red/green hunks. Make ALL file changes with mcp__lemma__edit or mcp__lemma__write; never write files with Bash or other tools. mcp__lemma__read_file returns the live editor buffer, including unsaved changes. Build with mcp__lemma__compile and read errors with mcp__lemma__get_diagnostics instead of running latexmk. Tool results may contain a <review_feedback> block listing hunks the user accepted, rejected or modified; do not re-apply rejected changes unless asked. Follow the project's existing macros and style.
- **MCP server:** newline-delimited JSON-RPC 2.0.
  - **Handshake replies:**
    - `server/discover` → result `{}` (the verified path);
    - `initialize` → `{"protocolVersion": <the requested version if in PROTOCOL_VERSIONS else "2025-11-25">, "capabilities": {"tools": {}}, "serverInfo": {"name": "lemma", "version": VERSION}}`;
    - `notifications/*` → no reply;
    - `ping` → `{}`;
    - anything else → error −32601.
  - **Connection:** the first line on each socket connection must be `{"lemma_token":"<32 hex>"}`; otherwise the connection is closed silently.
  - **Tools** (arguments mirror Claude's built-in tools):

    | Tool | Arguments | Result |
    |---|---|---|
    | `read_file` | `file_path`, `offset?`, `limit?` (default 2000) | `cat -n` text of the live buffer; lines over 2000 characters truncated |
    | `edit` | `file_path`, `old_string`, `new_string`, `replace_all?` | See §4.2 |
    | `write` | `file_path`, `content` | — |
    | `list_files` | — | Project graph plus other `.tex`/`.bib`/`.sty` under the root (at most 500) |
    | `outline` | `file_path?` | Sections with line numbers and labels |
    | `compile` | — | Status, at most 20 errors, warning count, pages |
    | `get_diagnostics` | `file_path?` | — |
    | `get_selection` | — | File, line/column, selected text (at most 20k characters) |
    | `render_page` | `page` | PNG at 1.5×, max 1600 px tall, as image content, **plus** text naming a PNG copy in `$XDG_RUNTIME_DIR/lemma/` (images reach the model: **verified 2026-10-11**; the model read a word visible only in the rendered page) |
    | `get_review_state` | — | Unified diff of pending hunks (at most 50 changes or 12 KiB per file, about 48 KiB in all; the rest counted with its lines) plus recent events |

  - **Paths:** each resolved through `ProjectRoot::resolve`; refusal text: `"path outside project root"`.
- **Permission card keys:** `y` = first `allow_once` option, `Y` = `allow_always`, `n` = `reject_once`. Cancel answers every pending request with `Cancelled`.

### 10.5 GUI module breakdown (`crates/lemma-app/src/`)

| File | Owner WP | Responsibility and key items |
|---|---|---|
| `main.rs`, `app.rs` | 0.2 | CLI `lemma [FILE] [--snapshot PNG] [--quit-after-ms N] [--mem-report] [--demo review\|pdf\|agent\|completion] [--apply-as-agent JSON]`; mallopt; tracing; `adw::Application` (id `io.github.lemma_editor.Lemma`); actions and accelerators |
| `window.rs` | 0.2 | `adw::ApplicationWindow`: header bar (build state, review summary), `adw::OverlaySplitView` (left: outline/changes; right: agent), `gtk::Paned` editor\|PDF, status bar, `adw::ToastOverlay`, `adw::Banner` |
| `workspace.rs` | 0.2 | `Workspace { root: ProjectRoot, docs: RefCell<HashMap<PathBuf, DocRef>>, index: RefCell<ProjectIndex>, … }`; `open(&Path)`, `get_or_load(&Path) -> impl Future<Output=Result<DocRef,String>>`, `save_all_dirty()`; turn snapshot and FileMonitor hooks (bodies added in 2.4) |
| `document.rs` | 0.2 | `Document { kind: Open{buffer: sourceview5::Buffer} \| Closed{shadow: String}, line_index, line_states, origin: Cell<Origin>, review: RefCell<Option<FileReview>> }`; `text()`, `with_origin(o, f)`, `apply_edits(&[TextEdit], Origin)` (one user action, descending), `byte_offset(&TextIter)`, `iter_at_byte(usize)`, `save() -> impl Future`, `connect_lines_changed(Fn(first, removed, added))`, `connect_hunks_changed(Fn())`; undo/redo History wiring [V pattern]; `on_edit` calls |
| `saver.rs` | 0.2 | Per-document coalesced async save (FileSaver; `replace_contents_future` for closed docs); remembers hashes of our own writes |
| `editor/tex_view.rs` | 0.2 | `TexView` (subclass of `sourceview5::View`); `snapshot_layer(AboveText)` → `review_overlay::paint(self, &snapshot)` |
| `editor/vim.rs` | 0.2 (2.10) | VimIMContext via capture-phase `EventControllerKey` + `set_client_widget`; `connect_write`; ex commands |
| `editor/review_overlay.rs`, `editor/review_gutter.rs` | 1.2 | Anchor tags, layout cache, `paint`, `ReviewBar` pool (8), positioning; `ReviewGutter` |
| `review_ui.rs`, `changes_panel.rs`, `agent_edits.rs`, `testmode/review.rs` | 1.2 | `ReviewController` (actions of §4.3, banner, toasts, auto-advance); `agent_edits::apply(ws, call, EditorRequest) -> impl Future<Output=ToolReply>` for ReadFile/Edit/Write (also used by M2 tools); `--apply-as-agent` JSON = `[{"tool":"edit","file_path":…,"old_string":…,"new_string":…,"replace_all":false},{"tool":"write",…}]` |
| `pdf/pdf_view.rs`, `pdf/pdf_controller.rs`, `build_ui.rs`, `diagnostics_panel.rs`, `testmode/pdf.rs` | 1.6 | PdfView, generations, sync UI, `BuildController::request(Reason)`, marks/underlines, Problems list |
| `editor/snippets.rs` | 1.8 (2.9) | `build_gsv_snippet(&[Chunk]) -> sourceview5::Snippet`: Text → `set_text` + `set_text_set(true)`, focus −1; Stop → focus n, plus `set_text` + `set_text_set(true)` if there is a default; Mirror → `set_spec("$n")`, focus −1. Expansion inside one user action. |
| `editor/{highlight,completion,typing,spell,keys}.rs`, `agent/{bridge,tools,panel,markdown,install}.rs`, `outline_panel.rs`, `editor/hover.rs` | M2/M3 WPs | §5, §4 |
| `memreport.rs`, `testmode/mod.rs` | 0.2 | `smaps_rollup`; `WidgetPaintable` → `renderer().render_texture()` → PNG |

### 10.6 Fixtures (copied in WP0.3)

The fixtures were copied from the projects built during the design research.

| Fixture | Source |
|---|---|
| `tests/fixtures/proj/` | `pdf_build/proj/*.tex`, `refs.bib`, `chapters/`, and the built `main.pdf` + `main.synctex.gz` (they contain absolute build paths; tests compare path suffixes) |
| `tests/fixtures/logs/<variant>/` | `pdf_build/variants/*/{main.log,main.79.log,*.blg,run1.log}` |
| `tests/fixtures/pdf/` | `pdf_build/stress/stress.pdf`, `gui/pdf/test.pdf` |
| `tests/fixtures/large.tex` | `gui/pdf/large.tex` (60k lines) |
| `tests/fixtures/review/` | New `main.tex` + `edits.json` covering the WP1.2 cases |

- `scripts/build-fixtures.sh` generates a 50-page `pdf50.tex` (lipsum + math) and builds it.
- Perf tests also read `LEMMA_PERF_CORPUS` (a local corpus) when it is set.
- Bib fixtures: `kpsewhich font.bib tugboat.bib` (system TeX Live).

### 10.7 Work packages, M0 and M1

**WP0.1 Workspace and contracts** (single integrator)
- **Owns:** root `Cargo.toml`, `.gitignore`, `LICENSE`, every `crates/*/Cargo.toml` and `lib.rs`/`main.rs` stub with exactly the §10.3 API, `crates/lemma-synctex/build.rs`, `crates/lemma-mcp/src/main.rs` (full: std-only bridge, env `LEMMA_MCP_SOCKET`/`LEMMA_MCP_TOKEN`, first line `{"lemma_token":…}`, two copy loops), and **all of `crates/lemma-core`**.
- **Depends on:** nothing.
- **Task:** create the workspace verbatim and implement lemma-core completely.
- **Acceptance:**
  - `cargo check --workspace --all-targets`, clippy and fmt pass.
  - `cargo test -p lemma-core` includes:
    - TextEdit `apply`/`delta`, and `apply_all` in descending order;
    - a proptest that `LineIndex` after random `apply` equals `LineIndex::new(text_after)`, plus `line_delta` cases;
    - LatestSlot: latest wins, and `close` unblocks `take_blocking`;
    - `ProjectRoot::resolve`: relative path, inside absolute path, a `../` escape refused, a symlink escape refused, a non-existent file inside the root accepted;
    - fnv1a64("") = 0xcbf29ce484222325 and fnv1a64("a") = 0xaf63dc4c8601ec8c.
  - The bridge test (Rust integration test, socket at a short relative path) relays two requests and stops relaying after a bad token.

**WP0.2 App shell**
- **Owns:** `main.rs`, `app.rs`, `window.rs`, `workspace.rs`, `document.rs`, `saver.rs`, `editor/{mod,tex_view,vim}.rs`, `memreport.rs`, `testmode/mod.rs`, and empty stubs for every other §10.5 file with the listed hook signatures.
- **Depends on:** WP0.1.
- **Task:** §10.5 rows for 0.2.
  - Built-in `latex` language; scheme follows `adw::StyleManager`; ReviewGutter slot left empty (stock line numbers until WP1.2).
  - Open via the CLI argument or `gtk::FileDialog`; FileLoader/FileSaver.
  - Document signal wiring: LineIndex is kept exact; `undo`/`redo` before-handlers set `History`, after-handlers (`connect_closure(.., true, ..)`) reset it.
  - Config loading.
  - `--snapshot`, `--quit-after-ms`, `--mem-report`.
- **Acceptance:**
  1. `cargo run -p lemma-app -- tests/fixtures/proj/main.tex --snapshot $T/m0.png --quit-after-ms 1500` exits 0 and writes a PNG larger than 10 KB that shows highlighted text.
  2. `tests/gtk_document.rs`: 500 random insert/delete/undo/redo operations keep `LineIndex == LineIndex::new(buffer text)`; `History` is observed for every undo/redo sub-edit and `User` otherwise.
  3. Save round-trip is byte-identical.
  4. `--mem-report` on the fixture shows RSS ≤ 100 MB.
  5. Manual check recorded in the PR: in Vim mode, interactive `:w` saves (programmatic `execute_command(":w")` does not emit `write` [V]); if `write` does not fire interactively either, intercept `:w` in the key layer.

**WP0.3 Fixtures, scripts, CI**
- **Owns:** `tests/fixtures/**`, `scripts/{ci.sh,rss.sh,shot.sh,build-fixtures.sh,budgets.toml}`, `docs/testing.md`.
- **Depends on:** WP0.1.
- **Task:**
  - §10.6.
  - `shot.sh`: `grim -g "$(hyprctl -j activewindow | jq -r '"\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')" out.png`.
  - `rss.sh` runs `lemma --demo X --mem-report --quit-after-ms N` and fails above budget × 1.15.
- **Acceptance:** `scripts/ci.sh` passes on the M0 tree; `scripts/build-fixtures.sh` produces `target/fixtures/pdf50.pdf` (50 pages).

M1 work packages all depend on WP0.1. The GUI packages (1.2 and 1.6) also depend on WP0.2. They work against the frozen stubs and integrate when the crates they consume merge.

**WP1.1 Review core** (`crates/lemma-review/**`)
- **Task:** implement §10.3 lemma-review.
- **Acceptance** (`cargo test -p lemma-review`; proptest at ≥ 2000 cases, `PROPTEST_CASES=10000` locally):
  - **Properties:**
    1. After random agent edits, `accept_all` ⇒ baseline == current and `is_clean`.
    2. Applying `reject_all` edits ⇒ current == baseline.
    3. Random User edits outside hunks never create or alter hunks.
    4. A Reject followed by a simulated GTK undo (delete the restored range, insert the agent text at its start, both `History`) ⇒ a hunk with the same HunkId.
    5. IDs of untouched hunks survive accepting or rejecting another hunk that changes the line count.
    6. `from_persisted(to_persisted)` reproduces the hunks; a different disk text ⇒ `None`.
  - **Unit tests:**
    - `str_replace`: not found, not unique (with N), `replace_all`, minimal trimming, multibyte UTF-8;
    - `minimal_edits` round-trip; `unified_diff` golden;
    - pure insertion, pure deletion at start, middle and EOF, a file without a trailing newline, a whole-file replace, an empty baseline;
    - one-word change ⇒ WordSpans on both sides;
    - `undo_review_action` after accept and after reject.
  - **Perf:** `perf_recompute` < 2 ms with one hunk in `large.tex`, < 8 ms with 281 hunks in 7k lines.

**WP1.2 Review rendering and controller** (files in §10.5)
- **Task:** §4.3 and §4.4 UI on top of WP1.1, plus `agent_edits::apply` and `--apply-as-agent`.
- **Acceptance:**
  - Golden snapshots (`lemma tests/fixtures/review/main.tex --apply-as-agent tests/fixtures/review/<case>.json --snapshot …`) for:
    - a pure insertion; a pure deletion mid-file;
    - an EOF deletion without a trailing newline (drawn below the last line);
    - a hunk on line 1; a word-level change;
    - a wrapped long deleted line (wrap on, narrow window);
    - reading mode.
  - `tests/gtk_review.rs`:
    - Alt+Y removes the hunk and leaves the file on disk unchanged;
    - Alt+N restores the baseline region and the file is saved;
    - Ctrl+Z after Alt+N brings back the same HunkId;
    - Alt+U after Alt+Y brings the hunk back;
    - Alt+]/[ wraps across hunks; auto-advance works;
    - a 200-hunk document never has more than 8 bar widgets, and the bars never intersect text rectangles;
    - frame time ≤ 8 ms p99 while scrolling (frame clock, release build);
    - `agent_edits` returns errors for outside-root, not-found and not-unique cases;
    - disk equals the buffer after every Edit before the reply future resolves.

**WP1.3 Build and log** (`crates/lemma-build/**`)
- **Acceptance:**
  - insta snapshots of `parse_log` for every `logs/<variant>` (pdf, lua, xe and 79-column):
    - errininput → `chapters/yoneda.tex:26`, Undefined control sequence, with context;
    - missinginput → `main.tex:46`;
    - runaway → `main.tex:41`;
    - missingbrace → `main.tex:66`;
    - warns → the 7 entries from the research;
    - run1 → `rerun = true`;
    - biber → `refs.bib:37` error plus a missing-entry warning; bibtex → `refs.bib:36`.
  - `command_line` exact argv and env; `detect_engine` and `detect_root` tables; `BuildQueue` state machine.
  - Integration tests (skipped without latexmk):
    - `proj` builds in a tempdir with PDF and synctex output;
    - the `\include` mirror dirs work;
    - an infinite-`\loop` document is cancelled, and `kill(-pgid, 0)` returns ESRCH within 1 s;
    - a 2 s timeout fires.
  - `perf_parse` < 5 ms per 35 KB log.

**WP1.4 SyncTeX** (`crates/lemma-synctex/**`)
- **Acceptance:**
  - `forward(main.tex, 66)` → page 2, box (126.67, 415.05, 358.66, 9.70) ± 0.1 bp;
  - `forward(chapters/yoneda.tex, 26)` → page 2;
  - `inverse(2, 200, 420)` → `…main.tex:66`; `inverse(2, 200, 240)` → `…chapters/yoneda.tex:25`;
  - missing file → `None`;
  - 1000 queries < 100 ms.

**WP1.5 PDF core** (`crates/lemma-pdf/**`)
- **Acceptance:**
  - PageLayout round-trips (`to_view`/`to_page` to 1e-6; anchor round-trip; clamping; fit-width);
  - ByteLru never exceeds its cap and evicts in LRU order;
  - the worker on `stress.pdf`: `Want` of 3 pages at `scale_q = 16` yields 1224×1584 bitmaps with `data.len() == stride × height`;
  - latest wins: after `Want(B)`, at most one page of the earlier A is still rendered;
  - stale generations are dropped; Forward and Inverse go through synctex;
  - `perf_render` < 15 ms per stress page.

**WP1.6 PdfView, build wiring, diagnostics**
- **Acceptance:**
  - `--demo pdf --snapshot` shows 3 pages;
  - scrolling `pdf50.pdf`: no frame over 16 ms, and the cache stays ≤ 48 MiB (debug assert);
  - after a rebuild the anchor is preserved within 1 px;
  - Ctrl+wheel zoom is anchored at the pointer; the dark-mode toggle works; the log prints `surface.scale()`;
  - save → build → diagnostics appear in the panel, gutter marks and underlines; Stop cancels;
  - Ctrl+Alt+J scrolls to and flashes the box; Ctrl+click in the PDF opens the source line;
  - `rss.sh pdf50` ≤ 170 MB RSS.

**WP1.7 Scanner** (`crates/lemma-syntax/src/{lex,state,context,tokens}.rs`)
- **Acceptance:**
  - table tests for every §5 math rule, including `\item[(i)]` (no error token), `\verb|$|`, `\$`, a blank line ending `$`, tikzcd as non-math, and a user math environment;
  - proptest: random LaTeX insertions and deletions on the fixtures ⇒ `apply_edit` states equal a fresh `rebuild`;
  - perf: `rebuild(large.tex)` < 15 ms; `context_at` < 50 µs.

**WP1.8 Snippets** (`crates/lemma-snippets/**`, `data/snippets.default.toml`, `editor/snippets.rs::build_gsv_snippet`)
- **Acceptance:**
  - `parse_body`: `$$1$$0` → `[Text("$"), Stop1, Text("$"), Stop0]`; `${1:0.8}`; mirrors; `${$}`; implicit `$0`;
  - the defaults load exactly 91 snippets (26/2/54/9), ported from a LuaSnip file;
  - fire/no-fire table for all 91: `xsr` ✗, `x sr` ✓, `)sr` ✓, `\sum` ✗, `a__` ✓, `mk` in a comment ✗;
  - `with_indent`;
  - `tests/gtk_snippets.rs`: every default is pushed on a 4-space-indented line and "X" is typed. The buffer must equal the expected expansion (first stop filled, mirrors updated, indentation applied), and the chunk focus positions must equal the expected list.

**WP1.9 Structure pass** (`crates/lemma-syntax/src/{structure,bib,aux,magic,project}.rs`)
- **Acceptance:** fixture tests for:
  - the outline (KOMA, starred commands, short titles, theorem environments);
  - labels with environment and title; cleveref lists and `\crefrange`;
  - biblatex `\cites(..)(..)[..]{a}[..]{b}` and natbib forms;
  - every include kind; the macro definition forms;
  - the magic-comment regexes from the research;
  - environment-pair recovery;
  - `scan_bib(font.bib)` returns entries despite `@Preamble` `#` concatenation;
  - `parse_aux`;
  - `perf_index` < 5 ms on 7k lines.


## 11. Contract changes after v1.0 (M1 and M2 review passes, 2026-10-11)

The sections above describe the design as frozen in M0. These changes came out of the adversarial review passes. Where
they conflict with the sections above, this section wins.

- **Diff:** the line diff has no deadline (deterministic hunks); only the word diff keeps 8 ms. latexmk gets `./<root>.tex`.
- **Permissions (§10.4):**
  - `y` = allow once, **Shift+Y** = allow always, `n` = reject. Keys are ignored for 500 ms after a card takes focus by
    itself.
  - An agent that withdraws a request (`$/cancel_request`) gets −32800, and the card shows Cancelled
    (`AgentEvent::PermissionWithdrawn`).
  - A mode Lemma does not offer, if set during start-up, is switched back to `default` with a notice.
- **Tools (§10.4):**
  - `edit`/`write` refuse `latexmkrc`, `.latexmkrc` and hidden paths, because latexmk would run them as Perl without
    a prompt.
  - CRLF files are shown without `\r` and keep CRLF when edited.
  - `render_page` waits for a running build at most `build.timeout_s` + 10 s; its MCP timeout is compile + `RENDER_GRACE`.
  - A lone UTF-16 surrogate in a request is answered under its own id.
  - The token handshake timeout covers the whole first line.
  - Stale sockets and render copies are removed on start.
  - `get_review_state` (2026-10-25) writes each file's diff from the review's hunks
    (`lemma_review::review_diff`, recomputed first if stale; the format and `@@` grouping of
    `unified_diff`, which it equals when nothing is cut) and caps it: at most
    `REVIEW_STATE_FILE_CHANGES` (50) changes or `REVIEW_STATE_FILE_BYTES` (12 KiB) per file,
    then "… N more changes in FILE, lines A–B: read_file shows them"; a change too long to
    quote by itself (a file the agent wrote whole) is only counted the same way; past
    `REVIEW_STATE_BYTES` (48 KiB) of diff the other files are listed with their counts and line
    spans (at most 100, then a count). About 400 bytes per changed line, so a review of about
    250 changes went past Claude Code's 25k-token limit for a tool's result; 8000 changes in
    301 files made a 605 KiB reply. The tool's description says so.
- **Turns and watcher (§4.2):**
  - Only dirty buffers under the root are saved before a prompt.
  - Only Lemma's latest or running write counts as its own.
  - A dirty buffer changed on disk during a turn gets a three-way merge or the conflict bar.
  - Files over `max(snapshot_cap, 4 MiB)` are not reviewed.
  - After a local cancel, the next prompt waits for the old turn's `TurnEnded`.
  - Changes reported after a turn ends still trigger the turn-end build.
- **Review (§4.4, §4.5, §10.3):**
  - Reject Turn reverts only that turn's own edits inside mixed hunks; Keep Turn keeps only hunks that are that turn's
    alone.
  - Deciding a hunk takes the turns' unknown places around it out, but not those touching the
    hunks next to it, which may be about them (`park_around`, 2026-10-25): accepting one hunk
    made the next one, whose change was not known exactly, look like its turn's alone, so a
    Keep Turn after it kept it whole.
  - `PersistedReview` gains optional `turn_edits` / `unknown_turn_edits`.
  - A dirty document keeps the entry captured at its last save.
  - `replace_all` with tens of thousands of matches is linear (D-5, fixed 2026-10-25: see
    "Batched review feed" below).
- **Editor (§5):**
  - Completion, snippets and typing helpers run only in LaTeX documents.
  - Bracket type-over counts over the paragraph.
  - Nested snippets merge into one session.
  - In math, text-leaning commands rank lower (`data/completion/math-ok.txt`).
  - `MacroDef` gains `optional_args` / `optional_default`.
  - `[typing] replacements` (off by default): `...` typed in text becomes `\dots` through the
    snippet driver, an undo step of its own as an autosnippet is (`typing::replacement`).
  - Editing commands (`editor/editing.rs`): Toggle Comment, Bold, Italic and Emphasize, each one
    undo step on the active editor.
  - Find and replace in the current file (`find_bar.rs`, GtkSourceView's search): its
    replacements are `Origin::User` edits (a review treats them as typing), unlike the Search
    page's Replace All, which is a named review turn; live environment rename stays out of them.
    A new query selects its first match only once the search's idle scan has counted the
    matches, so typing the query never scans the file in one go (§3 rule 1). F3, Shift+F3 and
    Enter before that count look for their match in the background (2026-10-25:
    `forward_async`/`backward_async`, which wait for the idle scan to get there; a step that
    scanned on the spot took up to 47 ms on `large.tex` and 8 ms on 10 000 lines when the only
    match was near the end or there was none); the steps and Replaces asked for meanwhile
    follow in order (a Replace replaces the match the steps before it selected), and an edit, a
    new query or the end of the search drops them. Once counted, steps are synchronous as
    before.
- **Keys (§9.3):**
  - The key layer runs before VimIMContext (verified).
  - With Vim on and an editor focused, Ctrl+W, Ctrl+O and Ctrl+N (New File; Vim's next line) go
    to Vim (`keys::is_vim_accelerator_key`); the menu, the palette and the keys elsewhere still
    run Close Tab, Open and New File.
  - Bold (Ctrl+B) and Italic (Ctrl+I) are disabled while Vim is on, so those keys are Vim's
    (page up, jump forward), as `win.pdf-search` lets Ctrl+F through outside the PDF. Toggle
    Comment (Ctrl+/) works in every mode. With another text field focused (the agent's prompt,
    an entry), the editing commands' keys go to that field (verified on real keys).
  - The find bar's Ctrl+F, Ctrl+H, F3 and Shift+F3 are enabled only while an editor or the find
    bar has the focus and Vim is off, so Vim keeps Ctrl+F and Ctrl+H and the Search page its own
    Ctrl+H. An accelerator may be shared by actions that are never enabled together
    (`app::SHARED_ACCELS`), and GTK runs the enabled one: Ctrl+F is also `win.pdf-search`,
    enabled only while the PDF has the focus.
  - A key controller in the capture phase of a text view runs before the view's input method,
    so it leaves every key to the input method while the view shows preedit text
    (`preedit-changed`), as the snippet driver and the typing helpers do: the agent's prompt
    box sends on Enter and drives its suggestions only without preedit (verified on real keys
    with a dead key), and a box with preedit text is not empty, so a permission card does not
    take the focus from it.
  - The agent's prompt box recalls the last 50 prompts sent from it (per window, in memory)
    with Up and Down, in the empty box or in a recalled prompt nobody edited, on its first
    (last) line as wrapped; while suggestions show, Up and Down are theirs. They are plain keys
    of a text field that is not an editor, so neither Vim nor the accelerators are involved.
  - Go to Line (`win.goto-line`, Ctrl+G) is enabled under the find bar's rule, so Vim keeps
    Ctrl+G (`:42` goes to a line there) and the PDF's search entry its Ctrl+G (next match). Save
    As's Ctrl+Shift+S is no Vim key. The window's accelerators do not reach a dialog: in the
    pickers Ctrl+N stays Down (verified on real keys).
- **Memory (§7):**
  - The 60 s inactive cache trim is implemented, and closed tabs free their view and buffer.
  - Idle PSS depends on how many other GTK processes share the libraries. Under Broadway, idle measures about 92 MB RSS
    and 60–70 MB PSS (hunspell en_US-large alone is 8 MB), which is over the 50 MB PSS budget. `scripts/rss.sh` asserts
    RSS and PSS only on real Wayland (`LEMMA_RSS_WAYLAND=1`). The PSS budgets are to be re-checked there.
- **Real agent:** edit routing and image delivery verified with the real adapter (§9.2).
- **Prompt attachments (§10.3, §10.4):** `PromptBlock` gains `Image { uri, mime, data }` and
  `Document { uri, mime, data }` (`uri` is the file's `file://` URI, empty for a pasted image).
  - The agent pane attaches files from a file chooser (the paperclip), drops on the pane and
    Ctrl+V (an image, or a file list of local files) in the prompt box (`agent/attach.rs`).
    PNG/JPEG/GIF/WebP within 3.75 MiB (5 MiB as base64, the API's limit) and 8000 px are sent
    as they are; other images, and larger ones (files up to 64 MiB), are re-encoded as PNG on a
    worker and scaled down to 1568 px (then by ¾ steps) until they fit. PDFs up to 20 MiB become
    `Document`s with their bytes (the API's 32 MB request limit, base64), UTF-8 text up to
    512 KiB a `Resource`. Larger PDFs and text files, of any size, become `Document`s without
    bytes (only the first 4 KiB is read, to sniff the type): a link that ACP agents open with
    their own tools; with the API backend they are refused. Anything else is refused with a
    toast.
  - ACP: `Image` → an `image` block when the agent advertises `promptCapabilities.image`
    (Claude's adapter does), otherwise a `resource_link` to the file (a pasted image: a text
    note) and an `AgentEvent::Notice`; `Document` → a `resource_link` (Claude's adapter ignores
    blob resources; its Read tool opens the file).
  - API backend: `Image` → an `image` block, a PDF `Document` with bytes → a `document`
    block, any other `Document` → a note naming the file.
  - The fake agent's `_options.no_images` advertises `image: false`.
- **M5 (2026-10-12):**
  - The M4 row is cut. Its important parts (completion for packed packages, environment tools, the align helper and
    latexindent as hunks) are in M5.
  - Agent backends are `claude` (ACP), `api` (Messages API with the user's key from the GNOME keyring or
    `ANTHROPIC_API_KEY`) and `pi` (ACP through a pinned pi adapter, with ACP `fs/*` routed through the review). The
    API and Pi backends are tested only against fakes.
  - The Claude adapter no longer inherits `ANTHROPIC_API_KEY`.
  - More ACP agents (2026-10-19): `codex` (`@agentclientprotocol/codex-acp` 2.2.2, installed with
    `--omit=optional` like Claude's adapter, running the user's `codex` through `CODEX_PATH`),
    `grok` (`grok agent stdio`), `gemini` (`gemini --acp`), `opencode` (`opencode acp`), `copilot`
    (`copilot --acp`) and `custom`. All run as `AcpFlavor::Generic`; `[agent.<backend>]` tables
    override `command`, `args`, `env`, `name`, `model` and `effort`. `AgentConfig` takes
    `program`/`args`/`env` instead of `node`/`adapter_js`/`claude_exe`.
    - Modes: Claude Code keeps `default`/`plan` only; other agents keep their own modes except
      `ELEVATED_MODES` (`bypassPermissions`, `yolo`, Codex's `agent-full-access`, Claude's
      `acceptEdits`/`auto`), falling back to `default` or else their first offered mode.
    - Instructions: ACP has no system prompt and only Claude's adapter reads
      `_meta.systemPrompt.append`, so other agents get `GENERIC_APPEND` as the first block of the
      session's first prompt, inside `<lemma_instructions>`. If the agent answers that prompt
      with an error that is neither a cancel nor its death (a quota, an expired login), the
      next prompt carries them again (2026-10-25).
    - Agents that write files themselves (Codex's `apply_patch`, …) are covered by the file
      monitor and the turn snapshot; the tool cards recognise Lemma's tools under other naming
      schemes (`mcp.lemma.edit`).
    - The effort picker and `agent.effort` use `effort` or, failing that, `reasoning_effort`.
  - Vim mode is off by default.
  - The default renderer is cairo (`[editor] renderer`).
  - The memory budgets are re-based on real-Wayland measurements (`scripts/budgets.toml`).
- **Reload after a build (§6), 2026-10-25:** the PDF is reopened with a new generation only when
  it changed. latexmk with nothing to do ("All targets are up-to-date": a save without changes,
  an agent's repeated `compile`) runs no TeX and leaves the last run's log, which still says
  `Output written`. So the build thread stamps the PDF (`BuildOutcome::pdf_stamp`: its size and
  its modification time to the nanosecond; none when that time is a whole second, as on FAT or
  HFS+), and `PdfController::open_if_changed` skips the open when the latest open asked for the
  same file with the same stamp. The main thread reads no file for it.
- **Builds (§6, 2026-10-25):**
  - Clean Build Files and Rebuild (`win.clean-build`, no default key) stops the running build
    and builds as F5 does, with
    `latexmk [-pdf|-lualatex|-xelatex] [-bibtex-cond1] -c -outdir=<outdir> ./<root>.tex`
    (`clean_command_line`: `-c` keeps the PDF and the SyncTeX file; `-bibtex-cond1` deletes
    the `.bbl` when its `.bib` files exist, and is left out when `outdir` is the root's
    directory, where a `.bbl` may be a source) run first on that build's thread, so no other
    latexmk runs beside it. The clean-up goes with that build: when it fails, that is the
    build's outcome; when the build cannot start, or is stopped before it starts, there is no
    clean-up; a Stop while it runs stops it, and there is no build.
  - Each build's toast replaces the previous build's (a success or Stop withdraws it).
  - A latexmk that does not start with `NotFound` is explained (`build_ui::LATEXMK_MISSING`:
    what to install, `[build] latexmk`): F5 every time, the other triggers once per window
    until a latexmk starts (the build thread reports it, also for `latexmk -c`; a build that
    ends without starting one does not count); the status bar says "Not built: latexmk not
    found".
  - Auto-save saves its documents together and then requests one `Save` build.
- **Project index (§5, 2026-10-25):**
  - The file monitor passes the changes of project sources and documents it sees to the
    project feed, outside turns too, before its own rules (which skip files that are not
    loaded): `completion::disk_changed` reads a file the index has from disk (not an open
    document; a bibliography even when it is open), or could not read, again on the jobs
    thread, and a file that is gone leaves the index (a directory that goes too: moved out of
    the project, its files get no events). Lemma's own writes (by content hash) are read only
    when the index could not read the file before. The paths of any kind created, deleted or
    moved in one debounce batch go to `completion::files_moved` once, which lists the project's
    files again only when the list lacks one that exists or has one that went, or a file under
    a directory that went (a save renamed onto its file changes nothing). Files read again do
    not count against the cap of files read from disk, and are read before new ones.
  - Each indexed file keeps the theorem names it was indexed with. When the project's names
    change (a preamble file read after the chapters), the files whose environments use a
    changed name are indexed again: open documents at once, the others on the next discovery
    pass. This converges, as a file's own declarations do not depend on the names.
  - The label checks and the Outline wait for `completion::index_busy` (re-index timers and
    jobs, discovery passes), not for the words rebuilt 1.5 s later; `project_busy` still counts
    everything, for tests that want the feed settled.
  - Local packages and classes join the index: for each `\usepackage`/`\RequirePackage{name}`
    of an indexed file, `name.sty` next to it and in every root's directory, and for a file
    with `\documentclass{class}`, `class.cls` next to it (`local_styles`), after the includes,
    so they take no budget from them. A package of the distribution is not there: its read
    fails and the path stays in `unreadable` until the next build. `prune` keeps the local
    packages (and what they include and load) of the files it keeps. Command completion leaves
    out macros whose names contain `@`.
  - The words of the `.tex` files read from disk are collected in one pass
    (`WordList::add_project_texts`: one merge into the sorted list) and only when their inputs
    changed: the dictionary, or the set of those files with a hash of each text as the
    discovery job read it. A pass that read only the `.aux` and the bibliographies, or a file
    whose text is unchanged, rebuilds just the open documents' words. The jobs do not read
    every new text of those files: when such a document closes (it may have been saved or
    reloaded while open) or Lemma saves it closed (an agent's edit, indexed from the saved
    text), the next pass that reads from disk collects the words again.
  - A re-index of an open document keeps the index's entry (its `Arc`) when the new index is
    equal to it (the job compares it with the entry the index had when the job started, which
    must still be there), the document class is the same, the job ran with the project's
    current theorem names (a declaration merged while it ran left it to its merge), and the
    project root and the open LaTeX documents are what the last `index_changed` followed:
    nothing is merged and no pass follows, only the words are rebuilt. The Outline compares
    the entries' `Arc`s, so prose typed recomputes nothing (2000 floats: 29 ms per pause
    before; 5 ms on `large.tex`). Rows that line shifts moved ahead of the index are
    recomputed once it is idle, even when it came back the same (two lines deleted and put
    back before it caught up).
  - `EnvPair::caption` (§10.3 addition): the first `\caption[short]{long}` or `\caption*` in
    the environment that is not inside a `sub…` environment within it, `short` unless it is
    empty, else `long`, cleaned as titles are. It goes to every open environment around the
    `\caption` that has none yet, up to the first `sub…` one, never to `document`. The
    Outline's float captions come from it (then from a label's title, then from the list of
    figures or tables) for open and closed files alike: no lines of the open document are read
    and there is no 40-line limit.
  - Files outside the workspace folder: `prune` records the directories of what is read from
    disk (the files it keeps but the open documents, unread includes too, and the
    bibliographies) as they are on disk: the discovery job normalizes each file and
    bibliography it reads or tries to read, and `Feed::real_paths` keeps where it is (a file
    no job read, past the cap, counts by the directory the index names). So a symbolic link in
    the folder to a folder or a file elsewhere (`refs` → `~/central`, `refs.bib` →
    `~/Zotero/export.bib`), which the folder's watcher does not follow, counts as outside.
    After every `index_changed` (and when the root moves) those outside the folder go to
    `watcher::watch_outside`, which watches the first `watcher::MAX_OUTSIDE_DIRS` (32) for the
    index only and drops the others. A source file changed, created or deleted there (or the
    directory deleted) goes to `completion::disk_changed` by its path on disk, which
    `keys_naming` matches with the index's paths (a key with the same file name normalized, a
    key whose recorded path is that one or under it: a file linked under another name, the
    files of a deleted folder that a link led to); nothing is reloaded or reviewed outside the
    folder.
  - The feed looks open documents up without the file system: `wanted` and
    `merge_discovered` compare the paths as the index names them with the open documents'
    keys; the discovery job normalizes each file it read (`ReadFile::key`), and a path that
    names an open document through a symbolic link goes to `unreadable` until that document's
    tab closes (`detach` takes the keys that name it out, so the next pass reads the file
    through the link). After a build the distribution packages, retried on the jobs thread,
    cost the main thread no `normalize` (52 with 40 packages before;
    `workspace::normalizations` counts them per thread).
  - The index replaces its parts and never changes one in place: §10.3's
    `ProjectIndex::aux` is an `Arc<AuxInfo>` (2026-10-25), replaced only when a build changed
    the numbers (`completion::merge_discovered` compares them). What is computed from the index
    tells what changed by the `Arc`s, holding a `Weak` (or the `Arc`) so that an address is not
    reused, without comparing contents:
    - The label checks keep `labels::scan::UndefinedCheck` from one check to the next: the
      entries they read (held, so that a replaced entry's labels come out of the counts), the
      bibliographies and the `.aux` (weakly), how many files define each label, the
      bibliographies' keys and each file's findings. A check that finds nothing replaced (prose
      typed) looks at nothing (`labels::checks_skipped`; the equation numbers still refresh,
      which costs nothing when their inputs are the same); otherwise it looks again at the
      references and citations of the files with new entries, at the references elsewhere to a
      label whose definition came or went, at every reference after a new `.aux` and at every
      citation after a bibliography was read again (`labels::files_checked`). Its findings are
      `scan::undefined`'s after every change (a differential unit test).
    - The equation numbers hash the `.aux` numbers once per `Arc` (`annotations::aux_hash`).
    - The Outline keeps what it computed for each file (`outline_panel::OutlineCache`, by path
      and whether the file is the root): its rows from its entry alone (`FileRows`, the entry
      held weakly), the numbers of their labels per `.aux`, the files its includes name while
      the index has the same files, and the rows assembled under it with what they were
      assembled from. The numbers from the contents lines, which follow the order of the whole
      project, are matched again for every outline over prepared keys (`AuxKeys`), and what
      they give is kept per file as overrides. A file whose rows, numbers and included files are
      as before keeps the same `Rc` of rows under it, and the patch skips the nodes under a node
      that shows the same `Rc` (`Node::shown`); only the row lists of the files whose rows came,
      went or moved are made again (`index_rows_of`). When the entries the index replaced give
      the rows they gave but for their lines (Enter typed, a `\ref` or a `\label` added to a
      paragraph) and the same includes, and the rows are on the new lines (`rows_followed`:
      they followed the line changes in a tab; not after another program changed a file that is
      not open, or after a patch put them back on the index's lines before it caught up),
      nothing is computed or patched (`OutlineStats::line_shifts`). `drifted` lists the files
      with rows on the lines a change replaced, moved at a guess (Enter at the end of a
      heading's line), not those whose rows only moved after a change, which is exact; once
      the index has caught up their rows are compared with its lines too, and only rows off
      them make the next outline look at every row. When only rows changed, the root is not
      looked for again.
    - Measured on a generated book (`tests/common/big_project.rs`: 300 chapters, 142k lines,
      20k labels, 30k references, 40k citations, a 10k-entry bibliography and an `.aux` that
      numbers every label; release, on the CPU, the median of five runs' medians at load 1–9):
      the check after a pause took 28.5 ms after prose, 34.4 after Enter, 22.3 after a `\ref`
      and 22.2 after a `\label`, now 0.08, 0.73, 0.51 and 0.80 ms; the Outline's look at the
      index once the chapter was indexed again took 40.2 ms after Enter and 33.6 after a new
      `\section`, now 0.16 and 3.7 ms (`perf_undefined_check_big_project`,
      `perf_outline_big_project`). Keeping what it computed makes the first outline of that
      book a little slower (median 77 ms instead of 67, wall time).
  - A window reads at most `completion::MAX_DISK_FILES` (300) files from disk. When the last
    discovery pass left files the project includes unread (`completion::index_capped`), the
    index lacks part of the project, so a reference to a label it does not have may well be
    defined: the label checks then report no undefined reference (`Undefined::labels_checked`,
    `labels::labels_checked`; citations are still checked), and the log says so once per
    window. The cap stays: completion, F12 and the Outline see only the files read.
  - The equation numbers follow line changes at `HIGH_IDLE`, which runs before GTK lays out and
    draws the next frame, instead of inside each change: the changes of one main-loop
    iteration (a paste, a replacement of several lines, Vim's `.`) add them again once, and
    nothing moves when the changes are below the last number. With 5000 numbers
    (`perf_equation_numbers_follow_lines`, release, on the CPU, five runs at load 1.5–2.4) a
    line break above them spends 0.07 ms in the change instead of 2.6 ms, and ten line changes
    at once cost 3.0 ms instead of 26; one line break with what runs before the next frame
    still costs about 2.6 ms (every number is added again).
- **Batched review feed (§4.1, §10.3, 2026-10-25):**
  - `FileReview::on_edits(edits, origin)` takes the changes of one action at once (in the
    order they are applied, each ending at or before the start of the one before, with the
    pre-edit offsets of its moment). The result is exactly that of `on_edit` for each in turn,
    which is what it does for User, History and Review edits; a differential proptest
    (`review::tests::on_edits_is_on_edit_for_each_edit`) compares every field of the review but
    the word spans (their 8 ms deadline).
  - For Agent and Reload edits it costs O(log hunks) per edit instead of O(hunks): before each
    edit it sets aside the hunks, segments and attribution intervals beyond the edit's reach
    (what `on_edit` reads for it: the hunks and segments it meets, `MAX_SLIDE` bytes past them,
    and whatever reaches across that), keeping the first hunk and segment beyond, and brings
    them back at the end moved by the edits since (`review/batch.rs`). Measured in release (on a
    loaded machine): a replace_all of `$` with 20 000 matches 2.2 s → 0.11–0.15 s; 5000 matches
    over 5000 hunks 0.76 s → 0.05 s; 20 000 right after Accept all of 5000 hunks 5.0 s → 0.15 s.
  - Alt+U of an Accept all puts its regions back the same way (`unaccept_batch`, from the last
    one down, with what lies beyond each set aside and its baseline offsets moved when it comes
    back), with the result of putting them back one by one (differential proptest
    `review::tests::unaccept_batch_is_unaccept_each`): 5000 hunks 0.27 s → 21 ms.
  - Accept all, an accept of several hunks and Keep Turn (`accept_many`, 2026-10-25) build the
    new baseline in one pass (its gaps and the picked hunks' texts) and move the baseline
    offsets and lines of the hunks left by a running sum. Then they take the picked hunks out
    from the last one down, as accepting each did (`park_around`, the records, the attribution,
    `prune_near`); of all that only `park_around` read the baseline, for the text of the
    segments around a hunk, which it now reads from the checked `current`. The result is that
    of accepting them one by one (differential proptest
    `review::tests::accept_many_is_accept_each`, also after a recompute and after Alt+U).
    Accept all on `large.tex` (release, medians of five runs in turn with the old code, load
    8–10): 5001 hunks 153 ms → 10 ms, 20 001 hunks 521 ms → 50 ms, about what Reject all takes
    (9 and 44 ms); Keep Turn 172 ms → 14 ms and 553 ms → 64 ms (`perf_accept_all_large_tex`).
    An accept of every other hunk still moves the hunks, segments and intervals left after each
    one it takes out of their lists: 2501 of 5001 hunks 108 ms → 29 ms, 10 001 of 20 001 hunks
    0.73 s → 0.44 s.
  - Word spans (2026-10-25): a recompute gives a hunk whose texts a hunk had at the recompute
    before that hunk's word spans, when those were refined before the 8 ms deadline
    (`Meta::exact_words`), and refines only the others (`LineDiff::refine_words`). A hunk is
    one op of similar's line diff (its `Replace` hook makes one op of each run between equal
    lines), and the inline diff reads that op's lines alone, so its spans depend on its texts
    only; spans refined past the deadline may be coarser, so they are refined again at the next
    recompute instead of being kept. The recompute after a keystroke outside the hunks of
    `large.tex` (the same runs): 5001 hunks 43 ms → 30 ms, 20 001 hunks 152 ms → 81 ms; at load
    5, 37–40 ms → 18–23 ms and 115–138 ms → 55–65 ms
    (`perf_recompute_keeps_word_spans_large_tex`). What is left is the line diff (about 15 ms)
    and making the hunks; the line diff is not windowed, so hunks still depend on the texts
    only.
  - The records that recognize the buffer's undo and redo of Lemma's own edits
    (`review/records.rs`) keep their ranges in an index (`Points`), and an edit takes out only
    the records it meets. A range an edit does not meet lies wholly before or after it, so each
    range is kept as its start and length, sorted by start into chunks of at most 128
    (`CHUNK_MAX`), each with a shift added to its starts. An edit moves the ranges after it in
    its own chunk one by one and the chunks after that as a whole: those are the tail, which has
    a shift of its own, so the edit only moves the tail's start to the chunk after its own,
    across the chunks between it and the edit before. The records it meets start between its
    start minus the longest range and its end, and each chunk knows how far its ranges reach.
    Searches start where the last one ended, and a full chunk hands a range at either end to the
    chunk before or to a new one instead of splitting, so a run of edits that each add a record
    at its end fills chunks without moving ranges between them (a replace_all in an open buffer,
    a deletion and an insertion a match, fills them about half: the newest turn's 32 768 records
    take about 500 chunks). An edit costs O(128) plus a step for each chunk between it and the
    edit before, and for each chunk the longest range could reach it from, however many records
    they hold. The index first kept the ranges split at the last edit, as `LineIndex` keeps line
    starts, and converted the ends between an edit and the one before one by one: with the
    newest turn's 32 768 records held (a replace_all of 30 000 matches in an open 10 000-line
    file), the first keystroke far from the last edit took 10–12 ms, 11–14 ms with the changes
    in one hunk; now 45–60 µs and 0.33–0.5 ms, as much as without the records within the noise
    (`perf_keystroke_after_a_jump_with_turn_records`), and runs of edits got faster too (a
    replace_all of 5000 matches over 5000 hunks 28 ms → 17 ms). Against a scan of every record,
    20 000 edits right after rejecting 5000 hunks (the decision keeps all their records) take
    0.07 s instead of 0.73 s (6.5 s before the batched feed). A completed undo or redo step
    re-checks only the records it brought back (the others followed the text edit by edit; the
    unit tests check them all after every change), against the model without building strings
    (`current_text_is`), and `growth_before` sums from the nearer end of the hunk list. Reject
    all of 5000 line hunks, with its recompute: 2.3 s → 0.05 s; its Ctrl+Z 100 s → 0.07 s; its
    Alt+U 1.1 s → 0.18 s (`regress2_d` `perf_reject_all_of_5000_hunks_and_its_undo`).
  - Beyond the cap of 512 records, the agent edits of the newest turn (a run of agent edits
    attributed to one turn, whatever comes between them) are kept as the newest decision's
    rejects are, at most their last 32 768 (`MAX_TURN_RECORDS`; a few hundred bytes each). The
    buffer's undo and redo of a replace_all with more matches than the cap are recognized change
    by change: the undo sends `UndoneByUser` (it sent `UserEditedInHunk` for the changes
    whose records were gone) and the redo brings every hunk back (it folded those changes into
    the baseline). A per-action log of the agent's changes, recognizing them where no record
    does, was the alternative; the records also bring back the turns, segments and records
    that an undo or a redo restores, and with the index above they cost O(128) an edit in a
    run of edits, a few µs more after a jump.
  - Which record a History edit belongs to (2026-10-25): of the records whose step it fits, one
    in the middle of its step goes first, then one going the way the user went last; of
    several, the one the buffer steps first, which is the newest for an undo and the oldest of
    the undone ones for a redo (it redoes first what it undid last). Two agent insertions of one
    text, the second right after the first (`\beta` added to a line twice), leave their empty
    records at one place once undone; the newest took the first redo, and the second redo,
    which no record claimed then, went into the baseline (property 9 of `tests/properties.rs`
    found it at about 1000 cases; `tests/review.rs`
    `redo_of_equal_insertions_at_one_place_brings_both_back`).
  - Records without a buffer (2026-10-25): only the buffer's undo and redo take up an agent
    edit's record, or a reject's once it is undone, and they reach only the edits made in that
    buffer (a closed document's text goes into a new buffer as not undoable). So
    `FileReview::set_in_buffer(false)` (a closed document: `review_ui::start_review` and
    `adopt_review` tell a new review, `Document::close_buffer` and `open_in_buffer` a change)
    drops those records, with the ones kept in them, and while the text is in no buffer agent
    edits are not recorded and a reject's record goes once Alt+U undid it. What stays is what
    Alt+U takes up without a buffer: the records of rejects not undone (Reject, Reject all,
    Reject Turn; the turns and segments parked under them), and the rejects they cut. Measured
    with a counting allocator (release): the review of a closed document after a replace_all
    of 600 matches takes 342 KB instead of 600 KB (430 bytes a match; Replace in Project over
    50 such files: 17 MB instead of 30 MB), of 20 000 matches 11.5 MB instead of 20.1 MB; an
    open document's of 600 matches takes 846 KB, now 340 KB once its tab closes.
  - The records take at most about `MAX_RECORD_BYTES` (16 MiB; their texts and 400 bytes
    each): beyond it the oldest go, as beyond the cap of 512, and the newest turn's agent
    records are kept beyond the cap only that far, so `MAX_TURN_RECORDS` small ones (about
    14 MB) but 32 of the largest (`MAX_AGENT_RECORD_BYTES`). The newest decision's rejects are
    kept beyond it (as many as the hunks it decided, with their texts). 200 replacements of
    100 KiB blocks in an open buffer: the review took 185 MB, 60 MB of it records, now 140 MB.
  - Still O(edits × hunks): Review and History edits (a Reject all and its Alt+U, the buffer's
    undo and redo) reach `on_edit` one by one, each moving the hunks, segments and attribution
    after it, as typing does. Where they go down the text, each new hunk goes in front of the
    others: Alt+U of a Reject all of 5000 hunks 0.18 s; undo and redo of a replace_all of 5000
    matches in an open buffer (10 000 changes) 0.14 s and 0.21 s, four times that for twice the
    matches (`perf_undo_and_redo_of_a_replace_all`). An edit inside one hunk costs O(that
    hunk's text), as before, so a replace_all inside one hunk that spans most of the file stays
    quadratic. Alt+U of an Accept all still moves the baseline after each region
    (`String::replace_range`): about 0.25 s for 20 000 hunks in a 1 MB file (Accept all itself
    builds the baseline once, above).
  - `Document::apply_edits` gives the review the changes the document reported while it
    applied them, at once when they are applied (`review_ui::before_edits`): a delete and an
    insert per edit on an open buffer, the edits themselves on a closed document, in runs of one
    origin (an `apply_edits` that an `end-user-action` handler makes meanwhile, such as the
    environment rename's, keeps its own). Typing, undo and redo, and the find bar's Replace All
    (GtkSourceView's) still reach `on_edit` change by change. Nothing reads a review while a
    buffer signal runs.
  - Measured 2026-10-25 (release, loaded machine; `gtk_search`
    `perf_find_bar_replace_all_under_review_large_tex`): the find bar's Replace All of 20 000
    matches on `large.tex` takes 3.0 s with 5000 hunks under review and 0.58 s without a
    review. The review's part is `on_edit` of 40 000 user changes: 0.76 s without hunks (each
    folds into the baseline, `String::replace_range`, O(file)) and 2.1 s with 5000 (the growth
    of the hunks before each change is summed, and the hunks and intervals after it are moved:
    O(hunks)). `on_edits` batches only Agent and Reload edits, so handing it these user edits at
    once would give the same result at the same cost; a gain needs a batched path for user
    edits in the review (the baseline rebuilt once, the hunks beyond each edit set aside), not
    done.
  - A closed document's `apply_edits` builds the new text in one pass and its line index once,
    instead of per edit. Every change is still reported on its own (edit and change observers,
    the version, and the line delta, taken from the index before all the edits: each lies before
    the edits applied before it). No observer of a closed document reads its text while a change
    is reported (the build's line journal and the problems' line trackers count lines; the
    outline only schedules work), so one composed line notification was not used: it would move
    the problems between two edits to the first changed line. `apply_edits` of 20 000 matches
    under review in a 10 000-line file: 2.8 s → 0.13 s closed, 5.5 s → 0.45 s open.
  - `LineIndex` keeps the starts of the lines after the last edit as distances from the end of
    the text, which an edit before them does not change, so an edit converts only the lines
    between it and the previous edit (and moves the starts after it only when it changes the
    number of lines). The per-signal updates of the open buffer stay exact for `byte_offset`;
    a run of changes down the text (`apply_edits`) or up it (the find bar's Replace All, one
    delete and one insert per match) costs the lines in between. On `large.tex`, Replace All
    of 20 000 matches spent 0.24 s of its 0.92 s in line index updates, now 8 ms of 0.61 s.
- **Edits that close an open user action (§10.3, 2026-10-25):** `Document::apply_edits` with
  an origin other than `User` closes a user action left open (Vim's insert mode) before its
  edits, which runs the `end-user-action` handlers; live environment rename's gives the partner
  the name typed so far there, after the caller computed the edits against the text before.
  The document records the changes made while the action closes (`close_user_action`) and
  moves the edits past them as positions move (`move_past`: an edit at or after a change's end
  by its length difference, one at or before its start not at all), so they replace what they
  were computed for. When a change replaced or inserted text inside one of the edits, the call
  fails with `EditError::Conflict` and applies none of them (the agent's tool reports it; the
  handler's change stays). The action is opened again after the edits or the refusal. The
  `connect_before_apply` observers run after the close and get the edits as applied. (Before,
  the edits went in at the old offsets: one byte off after a star typed into `\begin{align}`.)
  Callers that record something before they apply such edits settle the action first
  (`Document::settle_user_action`: end it, so its handlers run, and begin it again) and only
  then read the text: the review's rejects (`review_ui::decide`, `decide_picked`,
  `decide_all_in`: a reject journals itself, parks its turns and queues the agent's `Rejected`
  feedback before its edits apply, and a refusal after that left the hunk without its turn,
  told the agent it was rejected and spent the Alt+U entry), Alt+U (`undo_in`, whose journal
  gives up its entry first) and the recovery bar's Review. A keep applies no edits and does not
  settle (Vim's insert session stays one undo step). `decide_picked` settles after
  `recompute_if_stale`: the review follows the handler's change edit by edit, so the hunk ids
  its caller picked stay valid.
- **Problems (§5, 2026-10-25):**
  - The Problems page builds a build's rows when it is shown, and lists at most
    `MAX_SECTION_ROWS` (500) of its problems, the most severe ones (errors, then warnings, then
    bad boxes, each in the list's order), then a line "N more not listed". The ones left out
    keep their gutter marks and underlines.
  - `win.problem-next` / `win.problem-prev` (F4 / Shift+F4; `]e` / `[e` in `[vim.normal]`) go
    from the cursor to the next or previous build problem in the list's order: the cursor's file
    first, then the files after (before) it, wrapping around, opening the file at the problem's
    mapped line as a click on its row does. Problems without a line, and findings, are skipped.
    Function keys are never typed, so the keys stay clear of Vim; Vim has no `]e` or `[e`.
  - The page is one `GtkListView` (`diagnostics_panel/rows.rs`) over a model of the build's
    rows and the sections' rows, one group (a `GtkSectionModel` section, whose header names
    the file, and a section's title above its first file) per file. The view makes widgets
    only for about 200 rows around the ones on screen, where the list boxes made one per row
    (up to 500 for the build and 500 per section); a change replaces only the groups that
    changed, one `items-changed` per run of them. A row's widgets are recycled: the details'
    state (shown or not, the quick fix once found) lives in the panel, not in them, and their
    toggle, details and "more" line are made when a row first needs them. The page lists no
    rows while it is hidden (the model is emptied when they change), as before. A list
    scrolled to its top is held there for three frames after an update: GTK 4.24 puts the new
    first row at the top, its file's header above it, out of sight.
  - `DiagnosticsPanel::set_findings_many` replaces the findings of many files of one source
    at once: one refill of the section, the open editors' findings applied once each, one
    refresh of the page. The label tools give it only the files whose undefined names changed
    since the last check (`labels::check`'s `changed_paths`), each path normalized once;
    they gave every file's findings one `set_findings` at a time, each refilling the whole
    section (quadratic: 2–4 s for 400 findings in 40 files with the page shown, about 18 s for
    6700 in 300 files; now 25–50 ms). `clear_findings` is one batch too.
  - `problems_from` canonicalizes each file of a log once and sorts the problems by a rank per
    file instead of comparing paths: 24 000 diagnostics in 300 files take 39 ms instead of
    693 ms (at a load of 10–15) on the build thread, before the PDF opens. The build
    controller asks for the PDF before it gives the panel the problems.
- **Changes panel (§4.3, 2026-10-25):** a refresh while the page is hidden only remembers the
  model (and sets the header's Undo Last Turn button); the page builds it when it is shown, or
  when `rows()` or `groups()` asks (as the plain diff waited already). Shown, an update keeps groups by kind and rows by file: a count changes
  a row's subtitle in place, and only rows and groups that came or went are added or removed;
  a row's buttons look up its document when clicked. A focused widget that goes lets go of the
  focus first, which then goes to the first file row or the editor, as before. With 300 files
  under review, a decision or an agent write cost 100–120 ms of rebuilt rows, shown or not;
  now 1–2 ms.
- **Idle stop and `session/load` (§7, §10.3, §10.4, 2026-10-25):**
  - `AgentConfig` gains `load_session: Option<String>` and `AgentEvent::Ready` gains
    `loadable: bool` (the agent advertised `loadSession` in `initialize`). With
    `load_session` and a loadable agent, `session/load` replaces `session/new`, with the same
    `cwd`, MCP server and `_meta`. Until its answer, the updates that replay the conversation
    (user, agent and thought chunks, tool calls and their updates, plans) are dropped: the
    transcript shows that conversation already, and nothing of it becomes a tool card or
    reaches the editor. Modes, options, commands, usage and notices count. A refused load, or
    an agent that cannot load, gets `session/new` and a `Notice`. A loaded generic agent gets
    `GENERIC_APPEND` again with its first prompt. Claude's adapter 0.89.1 advertises
    `loadSession: true` (and `sessionCapabilities` `close`, `delete`, `fork`, `list`,
    `resume`, `subagents`); its `session/load` replays the history before it answers and
    reads `_meta` as `session/new` does. ACP 3.3.0 has the `session/load` types (and
    `session/resume`, which would replay nothing; not used).
  - The bridge arms a timer of `agent.idle_stop_min` minutes (default 10, 0 = never) when an
    ACP agent becomes `Idle` (its `Ready`, each turn's end) and ends it on a prompt, a
    permission request, a start and a stop; never for the API backend, which has no process.
    When it runs out with the agent still idle, nothing being prepared and no permission
    request pending, the agent is stopped as Stop does, without the panel's "stopped by the
    user" state, and a `Notice` says so, unless `panel::holds_agent` (text or attachments in
    the prompt box, or the pane shown in the focused window) or `agent.idle_stop_min` changed
    meanwhile, which start the timer again.
  - A loadable session that had a prompt is kept (`Resume`: its id, `cwd`, backend, mode,
    model and effort); each start in that root with that backend loads it, and the first
    `Ready` takes it (a start that fails leaves it for the next). When that `Ready` names the
    session, its mode, model and effort are chosen again instead of the configured ones. An
    agent whose conversation the next start could not continue (it cannot load sessions, or
    `config.toml` chose another backend while it ran) is stopped only while no prompt went to
    it. Any other stop forgets the session. The pane keeps the context usage when `Ready`
    names the session it showed last.
  - The idle stop's `Notice` promised to resume the conversation, so a `Ready` that does not
    continue it says why: after a start in another root (the root moved while the agent was
    stopped), the bridge emits a `Notice` after that `Ready` naming both directories; a load
    the agent refused, or an agent that cannot load, gets `lemma_agent`'s own `Notice`; a
    switch of backend stops the agent (which forgets the session), and the pane's notice says
    "It starts a new conversation with your next prompt".
  - Each `ToolRecord` keeps at most `TOOL_RECORD_TEXT` (2 KiB) of its reply's text, cut at a
    character boundary with `…`, and the whole length in `bytes`; a stop clears the records.
  - The API backend appends streamed text, thinking and signature deltas to their block in
    place (a block of n bytes in k deltas cost O(n·k) bytes of copying).
- **Files and lines (2026-10-25):**
  - Go to Line is quick open with `:` typed (`quick_open::parse_query`): `:N[:C]` in the active
    document, `path:N[:C]` in the files that match `path` (an exact path first). It goes through
    `goto::navigate` with `GotoTarget::LineColumn` (0-based; the column visual, as the status
    bar's; lines counted as Vim does, `goto::last_line`), so Alt+Left returns.
  - New File (`win.new-file`) and Save As (`win.save-as`; `:sav[eas][!] path`) are
    `file_commands`. New File opens the path chosen in a save dialog (a missing file is an empty
    document that its first save creates, an existing one opens as it is). Save As writes the
    text through a new closed document of the workspace (`Workspace::insert_closed`, the old
    document's charset, `DiskState::Absent`, so without `!` a file that exists is refused) whose
    saver records the write as Lemma's own, opens that document in the old tab's place and closes
    the old one discarding its unsaved changes (its file is untouched; undo history is not
    carried over). Refused while the agent is busy (`panel::is_busy`) and while the document has
    undecided hunks; a path that another tab, a review or unsaved changes of a closed document
    hold is refused too (a closed document holding only what is on disk is released first).
  - Vim's `:w path` (another file) writes a copy the same way and releases its document again;
    the current document stays as it is. It is also refused in the agent's grace window after a
    turn (`bridge::in_agent_window`), where the file monitor would review the new file as the
    agent's. `:wq path` writes the copy, then closes as `:q` (`:wq!` as `:q!`). GtkSourceView
    passes `:w`'s name on as typed, so `:w !cmd`, `:w >> file` and names with `%` or `#` are
    refused with a toast (`vim::unsupported_file_name`) rather than written as files of that name.
- **Start-up and sessions (§10, 2026-10-25):**
  - The command line is `lemma [[+LINE] FILE[:LINE[:COLUMN]]]... [OPTIONS]` (`cli::FileArg`).
    The first FILE decides the project root as before (also when it does not open: the root is
    then pinned, so the next document does not move it); only the first can be a directory (a
    directory later is a usage error, exit 2). `FILE:LINE` is a place only when no file has that
    very name; `+LINE` goes with the next FILE and wins over its `:LINE`. The first document is
    opened with `open_file`, the others with `WindowState::open_file_in_background` (a tab that
    is not selected, focused or drawn); a document with a line gets its cursor there, which the
    session restore then leaves alone (`session::keep_position`). The desktop entry passes `%F`.
    A tab saved before it was ever drawn keeps the first line its last session had only while
    its cursor is still on that session's line and the command line did not put it there;
    otherwise its first line is the cursor's, so a background `+LINE` tab is restored showing
    its line (it kept the old session's scroll, also when that did not show the same line).
  - The session restore opens its tabs in the background too, the saved active tab first (in a
    window without tabs the first tab opened is the one shown), so a tab not shown runs nothing
    that waits for its first frame (spell checking, completion's popup; §7) and the focus stays
    where it is. macOS's handed files: all but the last in the background.
  - While the start's documents open (the FILEs, the last session's file, or the tabs of the
    session that `restore_panes_early` read), the editor area shows a blank `opening` page
    instead of "No Document Open" (`WindowState::set_opening`).
  - Before the window is presented, `session::restore_panes_early` reads the project's session
    file synchronously (a few hundred bytes; an exception to §3 rule 2, as `last.json`, read at
    the same moment, and the settings files, read before the main loop, are) and applies the
    sidebar, its page and width, the PDF pane, its strip and the split, so the first frame has
    them; `restore` then uses what it read. The agent pane waits for `restore` (showing it
    starts the agent, and whether it is an overlay is known only after the first allocation).
  - `LastSession` gains `window` (`WindowGeometry`: GTK's default size, which a resize changes,
    and whether the window is maximized). Interactive windows apply it before they are shown
    (shrunk to the largest monitor) and record their own (`session::remember_window_size`); a
    window sized by `LEMMA_WINDOW_SIZE`, or a scripted run's, writes the remembered one
    unchanged (`keep_window_size`).
  - `session::prune_once`, 10 s after an interactive start, once per process, on the jobs
    thread: removes `*.tmp` files older than 10 minutes and `<hash>.json` sessions not written
    for 180 days or whose root is missing from a parent directory that still exists (a project
    whose parent is gone too, as on a removable drive that is not mounted, keeps its session);
    never `last.json` or the sessions of the open windows' roots.
- **PDF view (§6, §10.3), 2026-10-25:**
  - `ThumbReq` gains `dark`: the PDF thread colours a dark thumbnail (the strip's, a link
    preview's) with `dark::darken_bgra`, as it does a dark `PageKey` bitmap, so `PagePicture`
    draws it as it is. After a toggle the other mode's thumbnail stands in through the colour
    matrix until its replacement arrives, as the view's pages do; a thumbnail of the other mode
    that arrives late is not shown.
  - While a zoom waits for its re-render (`RERENDER_DELAY`), a page with no texture of the shown
    generation and mode to stand in (one that came into view as the zoom went out) is requested
    at the zoom's own scale, not at the old target scale (`Model::request_q`). A page with a
    stand-in keeps its scale and waits for the timer: the target scale as before, or that of a
    whole page or bands rendered earlier in the same zoom (a pinch, several Ctrl+wheel notches;
    bands of a scale between the target's and the zoom's), which is not asked for again at the
    old scale. So a page in bands gets the ones it lacks at the scale of its first, and
    `Model::fallback_tiles` draws them all (each notch used to ask for it again at its own
    scale, and only one scale's bands were drawn). A band asked for at one notch can arrive
    after the next notch asked for the page at its own scale: of bands of several scales, the
    page keeps those nearest the target scale, the ones `fallback_tiles` draws, and of a whole
    page and bands, the whole page. A page that is one bitmap at the zoom's scale while its
    tiles at the target scale do not cover it in view (the band rendered ahead below the view,
    the bands in view before the zoom went out, as after Ctrl+1 from far in) is requested whole
    at the zoom's scale, or keeps a whole page of another, rather than in more tiles at the old
    one. Meanwhile the band below the view is asked for only at the zoom's own scale
    (`Model::next_band`): one at the scale its page keeps would be rendered again by the
    re-render.
  - The zoom follows the view in one of three modes (`pdf_view::Fit`: width, page, none);
    `PageLayout::fit_page_zoom` fits the tallest page and its gaps to the height, at most the
    width's zoom. The session's `PdfPlace` gains `fit` (default width, so older files load; a
    mode a newer version wrote reads as none, which leaves the zoom to `zoom`, and without one
    fits the width).
  - `win.pdf-fit-page` (Fit PDF to Page), `win.pdf-zoom-reset` (Zoom PDF to 100%),
    `win.pdf-open-external` (Open PDF in Another App) and `win.pdf-show-in-folder` (Show PDF in
    Files) have no default keys; the toolbar's menu has them. The last two are disabled ("Build
    the document first") until a PDF is shown and hand it to `gtk::FileLauncher` (`launch`,
    `open_containing_folder`; GTK reveals a file through the portal or the freedesktop file
    manager on Linux and through Explorer on Windows, and has no way on macOS, where `open -R`
    does it). Scripted runs start no other application; tests set
    `PdfController::set_launch_hook`.
  - The view's own keys gain Ctrl+1 (100%), and Left and Right, `h` and `l` (sideways, taken
    also when there is nothing to scroll). They work only while the view has the focus, which no
    editor's Vim sees; Ctrl+1 is no accelerator (`gtk_shell_m3` checks it), like Ctrl+0.
  - **Tiles** (§6): a page over the 16 MiB threshold (or cairo's 32 767 px) is rendered in
    equally tall full-width bands of at most `TILE_BYTES` (4 MiB) instead of 512² tiles. Each
    tile is a poppler pass over the page's whole content stream (cairo clips the drawing), so
    fewer, larger tiles cost far less: a stress.pdf page at scale_q 26 (1989 × 2574 px, fit
    width in a HiDPI pane about 1000 px wide) takes 23–29 ms in its 5 bands, 77–93 ms in 24
    tiles of 512² and 10–12 ms whole (`lemma-pdf` `perf_tiled_page`, on a loaded machine). A
    page wider than `MAX_TILE_WIDTH` (4096 px) is zoomed in far and shows only part of its
    width, so it is cut into equal tiles of about 1024² (4 MiB) instead: a 1800 × 2000 px view
    in the middle of a stress page at scale_q 96 (7344 × 9504 px) takes 9 of them (33 MiB) and
    31–34 ms, where 25 tiles of 512² (25 MiB) took 77–78 ms and bands up to 4096 px wide 16
    tiles (63 MiB, more than the default cache) and 52 ms.
    `TILE_SIZE` is gone: `tile_size` gives a tiled page's tile size and `tiles_covering` the
    tiles under a region of its bitmap (the hover crops use it); tiles keep that shape at every
    scale, so an explicit tile of a page that is not tiled can still be rendered. The view asks
    for the visible bands, then for the band that scrolling down shows next while it fits in
    the cache (`geometry::tiles_below`: the next band of the last visible page or, once its last
    band is in view, the first of the page after it), then for the whole pages ahead; tiled
    pages were never rendered ahead before. Pages under the threshold are one bitmap as before.
  - **Dark pages** (§6): `dark::darken_bgra` looks up the output of white paper and of opaque
    grey (r = g = b) pixels whole, in tables built with the same integer arithmetic as every
    other opaque pixel's (three table lookups per channel), so the output is the same to the bit:
    `lemma-pdf` `dark_pages_come_out_as_before` compares it with the transform as it was, on
    every opaque colour and on rendered pages, light and dark. Text pages are 95–98% white and
    the rest grey: colouring a page takes 0.6 ms instead of 3.4–3.8 ms at scale_q 10 and
    2.7–2.9 ms instead of 15.5–16.2 ms at 21 (fit width on a HiDPI screen; `perf_dark_pages`),
    and through the PDF thread a dark pdf50 page at 21 renders in 7.5–9.3 ms instead of
    17.1–21.8 ms (5.7–7.5 ms light), at 10 as fast as a light one (load 5).
  - **SyncTeX** (§6, §7): the `.synctex.gz` is parsed on a helper thread (`synctex`, §3; about
    0.25 ms per page, 125–200 ms for 500 pages under load), which first frees the scanner it
    replaces, so memory never holds two (about 85 KiB per page: 47 MiB for 500). The PDF thread
    serves its commands and renders meanwhile. A Forward or Inverse that finds no scanner for the
    file as it is now (its stamp, compared on each query) waits, with the queries after it, and
    they are answered in order when the parse ends; a parse whose file changed meanwhile, or that
    a newer Open replaced, is dropped (its scanner freed on a thread of its own: freeing takes
    libsynctex's lock, which another parse may hold). After an Open whose SyncTeX file changed,
    the file is parsed right away only when a sync came since the previous Open or in the last
    five minutes (`RECENT_SYNC`); otherwise only the old scanner goes. A parse holds the lock
    throughout, so the label preview's PDF thread waits for it with its Forward queries. With a
    Ctrl+click sent before the Want of the visible pages of a 500-page rebuild, the first page
    came after 169–230 ms, now after 14–25 ms (13–17 ms without the click, load 5–9); the first
    Forward after a rebuild parsed ahead takes 0.8 ms instead of 126–190 ms
    (`perf_first_sync_after_a_rebuild`). Waiting queries survive a restart after a panic (they get
    a parse of their own; parses are numbered across restarts, so one left running is never
    taken for a newer one), and a parse that ends after the thread stopped frees its scanner
    itself.
  - `PdfCmd::Trim` (§10.3 addition) frees the scanner (and a parse ahead that no query waits
    for), runs the PDF thread's own `heap::trim`, and forgets the recent use, so the next rebuild
    parses nothing ahead. `WindowState::trim_memory` and hiding the PDF pane send it (not when the
    pane hides because the PDF moved to a window of its own; and `trim_memory`, which runs while
    the main window is in the background, leaves the PDF's textures and scanner alone while the
    PDF's own window is active). A process showing a 500-page PDF goes back from 74 MiB to
    27 MiB RSS after its first sync and a trim.
  - **Reloads** (§6, §10.3 addition): `PdfCmd::Reload` opens a rebuild of the current document.
    With as many pages, `PdfEvent::Opened` reports the current document's page sizes instead of
    reading every page first, which takes 0.03–0.1 ms a page (24 ms for 500 pages at load 7,
    0.6 s for 5892 at load 11–15). The pages are read afterwards, once no page waits to be
    rendered, 32 at a time in a unit of work that takes its turn with searches and thumbnails: a
    unit between the bands of the pages in view held them back (two pages at scale_q 26 in 12
    bands, pdf500, load 12–15: 145–160 ms to the last band after a Reload, 119–128 ms with the
    renders first). A page is always read before it is rendered (its tile grid) or its
    thumbnail is, mapped for SyncTeX (`box_to_display`, `point_from_display`) or its rotation
    used. When a page read has another size, `PdfEvent::PageSizes { generation,
    page_sizes }` (§10.3 addition: all of them) comes before any event that depends on it; a
    restart after a panic reads every page and reports them once. Another page count, another
    file, or `PdfCmd::Open` read every page first as before; the label preview's worker uses
    `Open`, as its crops take the sizes of `Opened`. The controller sends `Reload` for the file
    the latest open asked for; the view lays out the new sizes keeping its anchor
    (`PdfView::set_page_sizes`), and the strip's thumbnail rows take them. A 500-page build shows
    5.8 ms after its Reload instead of 24 ms, its first page after 23 ms instead of 40 ms
    (`perf_reload_of_a_long_pdf`).
  - **Bookmarks** (§6): a rebuild whose outline has the same titles in the same tree keeps the
    strip's rows; when only destinations (or `/Count` signs) changed, the entries take them in
    place. An entry is shared by the root list, the child lists the tree has made and its
    parent's children, from which a collapsed entry's list is made later. The rows' page labels
    follow (`refresh_bookmark_rows`), and so do the row the view selects and the jumps. Other
    outlines are built again, in one splice. With 500 bookmarks, the longest main-loop iteration
    after a rebuild took 57–73 ms (median) at load 9–11, now about 1 ms, the same outline or with
    moved destinations (`gtk_pdf_m3` `perf_bookmarks_after_rebuilds`, ≤ 5 ms; 46–50 ms when the
    titles changed).
- **Recovery of unsaved changes (§3, §7, 2026-10-25):** auto-save is off by default, and a
  crash, `SIGTERM`, `SIGHUP` or a logout used to end the process without the "Save changes?"
  dialog, losing every unsaved change (`recovery.rs`, `[editor] recovery`, default true).
  - The file: `Paths::recovery_dir()/<fnv1a64(normalized path) as 16 hex>.json`
    (`$XDG_STATE_HOME/lemma/recovery/`), 0600 in a 0700 directory, a `RecoveryFile`
    `{version: 1, path, disk_hash, saved_at, pid, text}`: `disk_hash` is the saver's (`fnv1a64`
    of the bytes the file held when the document last read or wrote it; `null` for a file that
    did not exist), `saved_at` Unix seconds, `pid` the writing process, `text` the document's
    text byte for byte (its line breaks; the charset is the file's, read again when the document
    opens). An optional `earlier: [{disk_hash, saved_at, text}]` (newest first, left out when
    empty) holds the texts the bar offered for the document and the user had not answered yet
    when the copy was written; it comes after `text`, or one of them takes the place of `text`
    when the document itself has no unsaved changes (`recovery::Snapshot`: `own`, `offered`).
    Only `path` and `text` are required when it is read. A process that writes recovery
    files holds an exclusive lock (`File::try_lock`: `flock`, `LockFileEx`) on `<pid>.lock` in
    the directory until it ends (`recovery::lock_file`), so a file whose writer still holds its
    lock is a copy of a document another Lemma (one process per window) has open, not one a
    crash left (`recovery::is_running`); a reused pid does not hold the lock.
  - Copies: 5 s (`recovery::DELAY`) after the last change to a document of the window, at the
    latest 30 s (`MAX_DELAY`) after the first change not copied yet, and when the window loses
    the focus, each edited document whose version changed since its last copy is copied on the
    main thread (`document::buffer_text`, one copy: under 1 ms for 10k lines, about 4.5 ms for
    `large.tex`; a keystroke only notes the time, §3 rule 1) and serialized and written on the
    jobs thread through a temporary file that is `sync_data`'d and renamed. One writer per
    process keeps the newest queued operation per file (a copy or a removal), so an older one
    never runs after a newer one. The copy goes when the document is clean again (its buffer's
    `modified-changed`: saved, reloaded, undone, `:q!`), when the user picks Discard in the
    "Save changes?" dialog (tab or window) or runs `:qa!` (`recovery::discard`, which also keeps
    that version from being copied again, as the window losing the focus while it closes would),
    and when `[editor] recovery` is turned off; the file goes with it unless it still holds
    offered texts (`queue_without_own`). A window's close (after the shell's close-request
    handler proceeds), its destroy and the end of `app::run` do the queued operations
    synchronously (like the session's close-time write, an exception to §3 rule 2). Nothing is
    copied or read in windows without `Paths` or that quit by themselves (`state.scripted`;
    `recovery::set_enabled` for tests with temporary directories).
  - Offers: a document that gets a tab without unsaved changes reads its file on the jobs thread
    (`recovery::decide`): another path (a hash collision), a text with U+0000 or a writer that
    still runs is ignored; of the file's texts (`text`, then `earlier`), those that equal the
    document's text or a newer one are dropped and the others are offered, newest first, one at
    a time (`recovery::Offer`); a file with none left is removed; an unreadable or corrupt file
    is logged and left alone. The session restore opens the tab of a file that is gone (a New
    File never saved) when such a copy of it is left (`recovery::is_left`), as an empty document
    that offers it. While a document's file is read, no copy of it is written. While it offers
    texts, the copies go on and keep the texts not answered yet after the document's own
    (`earlier`), so a crash, a kill or a signal before the answer loses neither the typing nor
    the recovered text; the next opening offers both. (Before, nothing was copied while a text
    was offered, and hours of typing next to an unanswered bar were lost with the process.) One
    bar per window (a revealer under the conflict bar, as the file monitor's) names the offered
    documents and how long ago their first offered texts were copied, and says when the file
    changed on disk since (the text's `disk_hash` is not the document's, as it is when the bar
    is shown: a save while the bar is up updates it). Review (`recovery::review`) takes each
    document's first offered text, settles a user action left open
    (`Document::settle_user_action`), and applies the text with `minimal_edits` against the
    document's current text as `Origin::Agent { call: 0 }` edits of one
    `review_ui::begin_named_turn` turn, "Recovered changes", then saves the documents that had
    no unsaved changes (§4.5: disk holds the proposal; quick fixes do the same); a document that
    refuses the edits (`EditError::Conflict`) keeps the text offered and in its file. None of a
    file's texts holds another's changes (each was copied while the others waited), and an
    agent-origin edit over hunks still to decide replaces their proposal, whose lines then go
    without a hunk (their baseline never had them). So a text stays offered, with a toast, while
    the document has hunks and Review applied another of its texts in this window
    (`Offer::reviewed`) or the file on disk is not the one the text was based on (after a save of
    the text reviewed before it, or across a restart). Discard takes the first offered texts
    out. After either, the file holds the document's text (while it has unsaved changes) and the
    texts still offered, which the bar names next. A tab closed while its file offers texts keeps
    them for its next opening.
  - Signals (Unix): for runs that do not quit by themselves, `app::run` attaches GLib Unix signal
    sources (`g_unix_signal_add_full`, declared in `recovery.rs`: glib 0.22 no longer wraps it)
    for `SIGTERM`, `SIGHUP` and `SIGINT` (Ctrl+C killed the process with its changes too), but
    not for a signal the process was started with ignored (`sigaction` reads `SIG_IGN`: `nohup`
    ignores `SIGHUP`), which GLib would otherwise take over, ending a `nohup lemma` when its
    terminal closed. The handler, on the main thread, writes every window's dirty documents
    synchronously (`recovery::write_now`) and closes the windows with
    `WindowState::close_without_asking` (the normal close: session, review state, the agent; no
    dialog); with no window yet, the app quits. A second signal while they close exits at once
    with 128 + the signal. Windows has no such signals: the timer and the copy on focus loss
    cover it.
  - `recovery::prune_once`, with the session sweep (10 s after an interactive start, once per
    process, on the jobs thread): removes `*.tmp` files older than 10 minutes, `*.lock` files
    as old that no process holds, and recovery files not written for 180 days.
- **Review overlay tags (§4.3, 2026-10-25):** anchors that reserve the same space share one
  anchor tag (`review_overlay::SpaceTag`: one per value of `pixels-above-lines`, one per value
  of `pixels-below-lines`; a tag's properties never change), so thousands of one-line hunks
  make a few anchor tags. There was a tag per hunk from a pool, pool tag i on the plan's anchor
  i: every tag in a buffer slows each of GTK's tag operations (the highlighter's and every
  keystroke's too; 25–100× with 5000 of them), a decision on hunk k moved the tags of the n − k
  anchors after it, and shrinking the pool after Accept all renumbered the priorities of the
  tags left for each tag it removed. A space no anchor reserves any more takes its tag out of
  the table, the highest priority first (`editor::remove_spacing_tag`); when a tag added to the
  table since outranks the anchor tags, they go back to the top.
  - Where a band is: on the plan's anchor line while the plan matches the text
    (`plan_version`). After an edit, until the recompute (typing: 50 ms), the review's hunks,
    which `on_edit` keeps exact, and the line index give the line (`ViewReview::groups_on`):
    a hunk's anchor line, or a later line of the hunk when text typed at its start went in
    front of the tag (GTK leaves text inserted where a range starts outside it), if that line
    starts with the tag of its group's space, which is where GTK reserves the space. The line
    is the one the plan's kind of anchor gives (a deletion at the end stays below the line
    before it while the user types after it), and a line holds one band above and one below
    it (when an edit joins two anchor lines that share a tag, the first group keeps it).
    Painting, the bars, the gutter's wedges and the cursor's outline all use it. A band whose
    hunk an edit merged into another, or whose line an edit joined to another anchor line, is
    not drawn until the recompute; there is no mark per hunk.
  - Re-tagging (`ViewReview::apply`) changes only what differs, for the tags of added lines
    and changed words as for the anchor tags. Each tag's ranges from the last apply are
    followed through the edits since: a `Document::connect_edit` observer records them as the
    spans of the current text they made, with their old lengths (`Edits`); a range no span
    meets or touches is where it was, shifted, and the tag's ranges in the regions the others
    may now cover are read from the text (`follow`, `tag_ranges`). Then the bytes the tag
    covers but should not are untagged and those it should cover but does not are tagged (a
    diff over bytes, so ranges that touch and are one in GTK need no care). Before the first
    apply, after a paste copied a tag (`tags_unsure`), and after edits in more than 128 places
    (`MAX_EDIT_SPANS`: a replace_all, a Reject all), the tag's ranges are read from the whole
    text instead. The tags of added lines and changed words used to be taken off the whole
    text and put back whenever their number of ranges changed (every decision) or one of them
    moved (typing in a hunk), and GTK drew or laid out again every line under them.
  - Measured (release, `gtk_review` `perf_review_many_hunks` and `perf_review_5000_hunks`,
    five interleaved runs each at a load average of 5–7, medians): with 3000 hunks in 9000
    lines, Alt+Y or Alt+N on the middle hunk 0.43 s → 20 ms (the main loop's work after it
    46 → 2 ms), the recompute after typing in a hunk 0.37 s → 18 ms, accepting the last hunk
    0.38 s → 20 ms, the sync with nothing to change 15 → 5 ms; with 5000 hunks in 15 000 lines,
    Accept all in the file 0.67 → 0.10 s, Keep Turn 0.64 → 0.10 s, Reject all 1.3 → 0.40 s
    and Reject Turn 2.2 → 0.38 s (their 10 000 buffer changes take 0.20 s without a review),
    the agent's write that makes them 1.1 → 0.56 s, a keystroke's p99 0.46 → 0.25 ms (0.05 ms
    without a review). Tag operations, which do not depend on the load (`gtk_review`
    `tags_change_only_where_the_hunks_do`, 200 hunks): accepting one 470 → 2, rejecting one
    462 → 2.
- **Opening and editing (§5, §7, 2026-10-25):**
  - A LaTeX buffer never gets GtkSourceView's `latex` language (`document::new_buffer`), not
    even until `highlight::attach`, which tells LaTeX documents by their file name
    (`completion::is_latex`). GtkSourceView frees a language's parsed definitions with the last
    buffer that uses it, so setting the language and taking it away again parsed and validated
    `latex.lang` and `def.lang` for every document opened (25–40 ms of the main thread each).
  - Vim's `:[range]s/pattern/replacement/[flags]` is Lemma's (`editor/vim/substitute.rs`,
    from `WindowState::vim_execute`) with GtkSourceView's meaning (PCRE2 patterns as typed, only
    `g` and `i`, GRegex replacements, GtkSourceView's ranges and cursor): all matches found in
    one pass over the text, one `Document::apply_edits(Origin::User)`, without live rename.
    GtkSourceView's loop searched the buffer again from its start after every replacement. For
    Vim's search pattern and jump list, GtkSourceView's own command runs first on the empty
    range `1,1`, which stops at the first match. A substitution that replaces nothing, and one
    Lemma cannot read (other separators, Vim's own marks), stay GtkSourceView's.
  - The highlighter's visible lines are at most 1.5 screens around the cursor when there are
    more of them than the view's height holds rows of its font (lines GTK has not measured yet
    count as empty: after a 1000-line paste all of them were "visible").
  - The word count's size limit (`AUTO_MAX_BYTES`) reads the root's includes from disk on the
    jobs thread (`wordcount::project_size`); the project index knew only the root when it was
    first shown, so large projects were counted. `wordcount::Stop` kills texcount from the main
    thread when the window goes or the application shuts down (`GApplication::shutdown`: a
    quit ends the process without destroying its windows).
  - A tab switch that looks for an earlier PDF (none shown yet) reuses the active document's
    root detection while no document's version changed (`build_ui::TabDetections`); a build
    always detects again.

## 12. macOS (2026-10-19)

Lemma builds and runs on macOS (Apple silicon, macOS 15 or later) against Homebrew's GTK stack,
which has every library at the pinned versions or newer. Linux stays the primary platform and
its behaviour is unchanged. The differences:

- **Allocator** (`lemma-app/src/heap.rs`): `heap::tune` is glibc's `mallopt` (nothing elsewhere);
  `heap::trim` is `malloc_trim(0)` on glibc and `malloc_zone_pressure_relief(NULL, 0)` on macOS.
  `forget_renderer_choice` leaves `GSK_RENDERER` set outside glibc, whose `unsetenv` alone is
  known not to free the string.
- **Processes**: `acp.rs`'s `group_has_members` asks libproc on macOS (`proc_listpids` with
  `PROC_PGRP_ONLY`, then `PROC_PIDTBSDINFO` to skip zombies) instead of reading `/proc`.
  `bridge::remove_stale_sockets` checks other instances with `kill(pid, 0)` on every platform
  (without `/proc`, macOS would have deleted the sockets of live instances).
- **API key** (`lemma-agent/src/keys.rs`): the login keychain through `/usr/bin/security`
  (service `lemma`, account `anthropic-api-key`). `security` takes a new password only in argv,
  so the store command goes to `security -i`'s stdin and the key is read back to confirm.
- **SyncTeX**: no package manager ships libsynctex alone on macOS, so `lemma-synctex/build.rs`
  compiles TeX Live's parser (`vendor/synctex/`, API 2.0.0, the same headers as the system
  library; MIT-style licence) with `cc` and links zlib.
- **Process environment** (`lemma-app/src/macos.rs`, first thing in `main`): inside `Lemma.app`
  the GLib/GTK/gdk-pixbuf variables point at the bundle (`GSETTINGS_SCHEMA_DIR`,
  `XDG_DATA_DIRS`, `GIO_MODULE_DIR`, `GTK_EXE_PREFIX`, `GDK_PIXBUF_MODULE_FILE` written from a
  template for the bundle's current location), so no module of a Homebrew installation (with
  its own GLib) is loaded. PATH gets the login shell's PATH (when not started from a terminal)
  and Homebrew's, MacTeX's and MacPorts' directories.
- **Finder**: on macOS the application has `HANDLES_OPEN`; files from Finder, the Dock or
  `open -a Lemma` open in the active window (the last one shown, the others in tabs behind it).
- **Not ported**: shortcuts stay on Ctrl (not Cmd); `--mem-report` reads `/proc` and reports
  that it cannot on macOS. The tests run on Linux only (`scripts/ci.sh`); some of them read
  `/proc`.
- **Packaging** (`packaging/macos/`, `.github/workflows/macos.yml`): `bundle.sh` builds
  `Lemma.app` with the libraries in `Contents/Resources/lib` (a directory named `lib`, because
  enchant relocates itself by replacing a trailing `lib`) rewritten to
  `@executable_path/../Resources/lib/` by `bundle_dylibs.py`, signs it ad hoc and makes the DMG.
  CI starts the app with Homebrew renamed (`smoke-test.sh`) and keeps snapshots of its window.
  `install.sh` is the one-command installer: the release's DMG (or a build from source), a
  `lemma` command and TeX (BasicTeX plus Lemma's tools and common packages, or MacTeX).

## 13. Windows (2026-10-24)

Lemma builds and runs on Windows 10 (1903 or later) and 11, x86_64, against MSYS2's CLANG64
packages (at the time of the port GTK 4.24, libadwaita 1.10, GtkSourceView 5.22, libspelling
0.4.10, poppler 26.08), for the `x86_64-pc-windows-gnullvm` target. It is built and packaged on the developer's
machine, natively in MSYS2 or as a cross build from Linux, never in CI. Linux and macOS keep
their behaviour; the Windows paths are `cfg`-gated. The differences:

- **Process groups → job objects** (`lemma-core/src/process.rs`): `Job::spawn` starts a program
  suspended, puts it in a new job object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) and resumes it,
  so nothing it starts escapes. latexmk (`lemma-build`), the math preview's TeX runs
  (`lemma-preview`), the agent (`acp.rs`) and npm run in one. Windows has no SIGTERM for these
  programs: stopping ends the job at once (exit code 1), after the agent's stdin was closed and
  `EXIT_GRACE` passed. `acp.rs`'s wait thread holds the agent's process handle until it may
  "reap" it by closing it, which keeps the pid from being reused as the zombie does on Unix.
  Every program Lemma starts gets `CREATE_NO_WINDOW` (`lemma_core::process::command`): Lemma is
  a GUI-subsystem program, and console programs would open windows of their own.
- **MCP transport** (`lemma-agent/src/transport.rs`): std has no unix sockets on Windows. The
  socket path names a file holding the loopback TCP address the server listens on
  (`127.0.0.1:<port>`); `lemma-mcp` reads it the same way. The token line, stale-file
  replacement (a file whose port no longer answers) and removal of files of dead instances work
  as before. A blocked socket read is not woken by `shutdown` on Windows, so connection reads
  wait `POLL_SLICE` at a time and see `stop`.
- **Paths**: `lemma_core::canonicalize` drops the `\\?\` prefix `fs::canonicalize` returns on
  Windows when the plain path names the same file; every canonical path in the workspace goes
  through it (workspace keys, `ProjectRoot`, SyncTeX matches). Windows resolves `..` by name
  before it looks at the disk, so `ProjectRoot::resolve` accepts `new/../x.tex` there (it is
  `x.tex`, still inside the root). latexmk gets `./<root>.tex` with `/` (TeX would read
  `\main` in `.\main.tex` as a command); drops insert `/`-separated paths; `TEXINPUTS` uses `;`.
  SyncTeX's `<cwd>/./<rel>` names may use either slash. The log parser (on every platform; Linux
  and macOS logs have no such paths) takes `file:line:` error headers whose path starts with a
  drive (`C:/…`, `C:\…`), as TeX names the distribution's files when it reports an error in
  one (an unknown package option).
- **Per-user directories** (`lemma-config/src/paths.rs`): `%APPDATA%\lemma` for the settings,
  `%LOCALAPPDATA%\lemma\{state,data,cache}` for the rest; an absolute `XDG_*` still wins (the
  tests rely on it). The default editor font is Consolas (Pango's `Monospace` alias is Courier
  New there).
- **API key** (`keys.rs`): a generic credential (`lemma/anthropic-api-key`) in the Credential
  Manager through `CredReadW`/`CredWriteW`/`CredDeleteW`; no program runs. The MCP token comes
  from `BCryptGenRandom`.
- **Programs** (`lemma-app/src/fsutil.rs`, `agent/install.rs`): a program is a file with an
  extension of `PATHEXT` (`node` → `node.exe`; npm's extensionless `sh` shims are skipped). npm
  runs as `node npm-cli.js` from node's own folder, through `std::process` in a job rather than
  GSubprocess, whose Windows spawn helper gives console programs a console window.
- **Process environment** (`lemma-app/src/windows.rs`, first thing in `main`): attaches to the
  console of a terminal that started Lemma (`--help`, the log) and appends TeX Live's, MiKTeX's,
  Strawberry Perl's, Node.js's, npm's, Git's and `%USERPROFILE%\.local\bin` to PATH when they
  exist and are missing. GTK finds its data next to its DLLs; nothing else is set.
- **The executable** (`lemma-app/build.rs`): the icon, version information and a manifest
  (`packaging/windows/lemma.manifest`: the UTF-8 code page, so the SyncTeX parser's narrow
  `fopen` opens any path, and long paths) are compiled with `windres` and linked into
  `lemma.exe`. The SyncTeX parser is the bundled one, as on macOS, linked with `shlwapi`.
- **poppler-glib** declares `poppler_certificate_info_new` but does not export it from its
  DLL; poppler-rs refers to it, and Windows linkers report it even unused. `lemma-core` defines
  a stand-in that nobody calls (`poppler_exports.rs`).
- **Not ported**: `--mem-report` (reads `/proc`). An agent that closes its stdout but keeps
  running is not noticed, because its children inherit the pipe's handle on Windows; an agent
  that dies is. The GTK tests run on Linux only (Broadway); the other crates' tests, and the
  app's unit tests, run on Windows (`packaging/windows/build.sh --test`, under Wine on Linux),
  with the Unix-only cases (shell scripts, `/proc`, symlinks, FIFOs, file modes) gated.
- **Packaging** (`packaging/windows/`): `build.sh` builds the release, gathers the DLLs by
  their import tables (`bundle_dlls.py`), the compiled schemas, icons, GtkSourceView data,
  pixbuf loaders with a relocatable `loaders.cache`, enchant (Hunspell and Windows' spell
  checker), fontconfig's configuration and the licences of the MSYS2 packages into
  `dist/Lemma`, zips it and makes an Inno Setup installer (`lemma.iss`). On Linux it downloads
  llvm-mingw, MSYS2's packages (`msys2.py`), a portable Wine and Inno Setup, each checked
  against a SHA-256, and Wine runs the Windows tools.
