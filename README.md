# Lemma

Lemma is a LaTeX editor for Linux and macOS (GTK 4 / libadwaita) built around reviewing an AI agent's
edits. When the agent changes your document, every change shows up inline as a red/green hunk
that you accept or reject on its own, like a code review, before anything is final.

It is also a complete LaTeX editor without the agent: continuous PDF preview with SyncTeX,
build diagnostics in plain English, completion, snippets, a project outline and more.

## Features

- **Agent edits as reviewable hunks.** Claude Code, Codex, Grok Build, Gemini CLI, OpenCode,
  GitHub Copilot, Pi or any other agent that speaks ACP, or the Anthropic API with your own key,
  edits the project through Lemma's own tools. Each edit becomes a hunk with word-level
  highlighting. Accept or reject per hunk (Alt+Y / Alt+N), per file or per
  turn, and switch to a reading mode that hides deletions (Alt+D). Ctrl+K edits just the
  selection or paragraph.
- **Attach images and files to a prompt.** Click the paperclip under the agent's prompt box, drop
  files or images on the agent pane, or paste (Ctrl+V) a screenshot or files copied in a file
  manager. Images (PNG, JPEG, GIF, WebP, and anything else GTK can read; files up to 64 MB) go
  to the model as images, scaled down when they are too large; PDFs and text files are attached
  too. The ACP agents take PDFs and text files of any size: large ones are sent as a link
  that the agent opens itself. With the API backend everything travels inside the request, so
  PDFs are limited to 20 MB and text files to 512 KB. A message may consist of attachments only.
- **Build and preview.** Saving runs `latexmk` (F5 builds by hand). The PDF pane reloads in
  place, supports dark mode, search, thumbnails and bookmarks, and SyncTeX works both ways
  (Ctrl+Alt+J forward, Ctrl+click in the PDF for inverse).
- **Diagnostics.** Errors, warnings and bad boxes from the TeX log and `.blg`, with
  explanations and quick fixes for common errors, plus optional `chktex` lint.
- **Writing help.** Completion for commands, environments, labels, citations and Unicode math
  glyphs from per-package word lists. Snippets and autosnippets, bracket pairing, spell
  checking, hover previews of `\ref`, `\cite` and rendered math, and label rename/references.
- **Navigation.** Project outline, command palette (Ctrl+Shift+P), quick open (Ctrl+P),
  project search and replace (Ctrl+Shift+F), go to definition (F12), session restore, local
  history and a git gutter.
- **Optional Vim mode** (Ctrl+Alt+V), off by default.

## Requirements

Lemma targets current GNOME 50-era libraries:

| Dependency | Version |
|---|---|
| GTK | 4.22 |
| libadwaita | 1.9 |
| GtkSourceView | 5.20 |
| libspelling | 0.4 (with enchant and a hunspell dictionary, e.g. `en_US`) |
| poppler-glib | 26.01 or newer |
| libsynctex | 2.0 |
| Rust | 1.92 or newer (to build) |

To build documents you need a TeX distribution with `latexmk` (e.g. TeX Live).

Optional tools turn features on when they are found: `chktex` (lint), `texcount` (word
count), `mylatexformat` (faster math previews) and `git` (the git gutter). The Claude, Codex
and Pi backends need `node` and `npm`, plus the agent itself, logged in (the `claude` or
`codex` CLI). The other agents only need their own command line tool.

Arch Linux ships all of these. Other distributions may not have libraries this new yet. On
macOS, Homebrew has them all (see [macOS](#macos) below).

## Installation

### Arch Linux (recommended)

The repository includes a `PKGBUILD` that builds the checkout you have:

```sh
git clone https://github.com/dharmavati213/lemma.git
cd lemma/packaging
makepkg -si
```

This installs `lemma` and `lemma-mcp` to `/usr/bin`, a desktop entry that opens `.tex` files, an
icon, and example configuration under `/usr/share/lemma/`. To make Lemma the default for `.tex`
files:

```sh
xdg-mime default io.github.lemma_editor.Lemma.desktop text/x-tex
```

If you installed Rust with rustup, see [`packaging/README.md`](packaging/README.md). It also
covers updating.

### macOS

Lemma runs on Macs with Apple silicon (M1 or newer) and macOS 15 Sequoia or later; Intel Macs
are not supported. macOS support is new, so please report what does not work. There are three
ways to install it.

#### Option 1: one command

Paste this into Terminal:

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/dharmavati213/lemma/main/packaging/macos/install.sh)"
```

The script installs Lemma in Applications, a `lemma` command in `/usr/local/bin`, and TeX. For
TeX it asks which you want:

- **Recommended** (about 0.6 GB): BasicTeX, plus everything Lemma uses (`latexmk`, `biber`,
  `chktex`, `texcount`, `latexindent`, `mylatexformat`) and the common LaTeX packages
  (AMS, TikZ, biblatex, siunitx, cleveref and the like).
- **Full**: all of MacTeX (7 GB download).
- **None**: you install TeX yourself.

It asks for your password to install TeX. A TeX Live that is already installed is kept and only
gets what Lemma is missing. If no ready-made app is published yet, the script offers to build
Lemma from source instead (Option 3, done for you). Options go after `bash -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/dharmavati213/lemma/main/packaging/macos/install.sh | bash -s -- --tex full
```

`--tex recommended|full|none`, `--from-source`, `--dmg FILE`, `--yes` (ask nothing) and
`--uninstall` (removes Lemma; settings and TeX stay). `--help` lists them all.

#### Option 2: the DMG

1. Download `Lemma-…-macos-arm64.dmg` from the
   [releases](https://github.com/dharmavati213/lemma/releases). (Every run of the
   [macOS workflow](https://github.com/dharmavati213/lemma/actions/workflows/macos.yml) also
   builds one: open a successful run and download the `Lemma-macos-arm64` artifact.)
2. Open the DMG and drag **Lemma** onto **Applications**. The app brings GTK and everything else
   it needs; you do not need Homebrew.
3. Start Lemma. The app is not notarized by Apple, so macOS blocks the first start: open
   **System Settings → Privacy & Security**, scroll down and click **Open Anyway** next to the
   message about Lemma. (Or run `xattr -dr com.apple.quarantine /Applications/Lemma.app` once.)
4. Install TeX to build documents (see [TeX on a Mac](#tex-on-a-mac) below).

To update, download the new DMG and replace the app in Applications.

#### Option 3: compile from source

You need Xcode's command line tools, [Homebrew](https://brew.sh) and Rust 1.92 or newer:

```sh
xcode-select --install                      # Xcode's command line tools (a dialog opens)
curl https://sh.rustup.rs -sSf | sh         # Rust (or: brew install rust)
```

Then install the libraries, build the app and copy it to Applications:

```sh
brew install gtk4 libadwaita gtksourceview5 libspelling poppler adwaita-icon-theme librsvg pkgconf
git clone https://github.com/dharmavati213/lemma.git
cd lemma
packaging/macos/bundle.sh                   # 5 to 15 minutes
cp -R dist/Lemma.app /Applications/
```

`packaging/macos/bundle.sh` runs `cargo build --release`, puts the binaries, GTK's libraries and
their data into `dist/Lemma.app`, and also makes `dist/Lemma-<version>-macos-arm64.dmg` to give
to others. An app you built yourself opens without the security prompt. To update, run
`git pull` and `packaging/macos/bundle.sh` again, then replace the app. Install TeX as below.
More on how the app is put together: [`packaging/macos/README.md`](packaging/macos/README.md).

#### TeX on a Mac

Option 1 installs TeX for you. Otherwise the simplest is [MacTeX](https://tug.org/mactex/),
which includes everything Lemma uses:

```sh
brew install --cask mactex-no-gui
```

The smaller BasicTeX works too, with Lemma's tools added:

```sh
brew install --cask basictex                # then open a new Terminal window
sudo tlmgr update --self
sudo tlmgr install latexmk biber biblatex chktex texcount latexindent mylatexformat
```

Add other packages the same way: `sudo tlmgr install tikz-cd`. The *Problems* panel names a
missing `.sty` file when a build needs one.

#### Using Lemma on a Mac

- Start Lemma from Applications or Launchpad, open a `.tex` file with **Open With → Lemma** in
  Finder, or use Terminal: `lemma path/to/main.tex` (installed by Option 1), `open -a Lemma
  main.tex`, or `/Applications/Lemma.app/Contents/MacOS/lemma` with the options under
  [Usage](#usage).
- Lemma finds MacTeX in `/Library/TeX/texbin`, Homebrew's tools and everything on your login
  shell's `PATH`, even when started from Finder.
- The agents that need Node (Claude, Codex, Pi) want `brew install node`.
- The API key is stored in the macOS Keychain.
- Shortcuts use **Ctrl**, as on Linux, not Cmd. macOS reserves some Ctrl shortcuts (Ctrl+arrow
  keys for Mission Control, Ctrl+Space for input sources) until you turn them off in **System
  Settings → Keyboard → Keyboard Shortcuts**.

### From source (any distribution)

Install the development packages for the libraries above, then:

```sh
git clone https://github.com/dharmavati213/lemma.git
cd lemma
cargo build --release --locked -p lemma-app -p lemma-mcp
```

The binaries are `target/release/lemma` and `target/release/lemma-mcp`. Keep them in the same
directory, because Lemma looks for `lemma-mcp` next to itself:

```sh
install -Dm755 target/release/lemma target/release/lemma-mcp -t ~/.local/bin/
```

Or run it from the checkout without installing:

```sh
cargo run --release -p lemma-app -- path/to/main.tex
```

## Usage

```sh
lemma path/to/main.tex     # open a document; its directory is the project
lemma path/to/project/     # open a project directory
lemma                      # empty window; open a file with Ctrl+O
```

Saving builds the document. F8 toggles the PDF pane, F9 the sidebar and F10 the agent pane.
Ctrl+? lists every shortcut, and Ctrl+, opens Preferences.

### Setting up the agent

Open the agent pane (F10) and choose a backend in its header:

- **Claude** (default): Claude Code through the ACP adapter. It uses your existing Claude Code
  login. The pane's **Install** button installs the adapter.
- **API**: the Anthropic Messages API with your own key. Click "Set API Key…" in the pane to
  store it in the GNOME keyring, or set `ANTHROPIC_API_KEY`.
- **Pi**: the `pi` coding agent through its ACP adapter. Run `pi` once to log in.
- **Codex**: OpenAI's Codex CLI through the ACP adapter `@agentclientprotocol/codex-acp`, which
  the pane's **Install** button installs. It runs your own `codex`; log in with `codex login`.
- **Grok Build** (`grok agent stdio`), **Gemini CLI** (`gemini --acp`), **OpenCode**
  (`opencode acp`) and **GitHub Copilot** (`copilot --acp`) speak ACP themselves: Lemma finds
  the tool on `PATH`, in `~/.local/bin` or in mise's installs. Log in with the tool first
  (`grok login`, …).
- **Custom**: any other agent that speaks ACP on stdio, from `[agent.custom]` (below).

Agent edits always arrive as hunks for review. Nothing is accepted for you. Agents that edit
through ACP's file methods (Pi, Gemini CLI) edit the live buffers; edits that an agent writes to
disk itself are picked up during the turn. Lemma never offers an agent's modes that skip its
permission prompts (`bypassPermissions`, `yolo`, Codex's full access, …).

How each ACP agent starts can be changed in `config.toml`; every key is optional:

```toml
[agent]
backend = "grok"

[agent.grok]
args = ["agent", "stdio", "--debug"]   # replaces the default arguments
model = "grok-4.7-build-fast"          # selected when the agent starts
effort = "high"

[agent.codex]
env = { CODEX_PATH = "/opt/codex/bin/codex" }

[agent.custom]
name = "Goose"
command = "goose"                      # a name on PATH, or an absolute path
args = ["acp"]
```

### Configuration

Settings live in `~/.config/lemma/config.toml`, with `snippets.toml` and `prompts.toml` next to
it. All three reload live. Preferences (Ctrl+,) writes the same file and keeps your comments.
The built-in defaults (`data/snippets.default.toml`, `data/prompts.default.toml`) are good
starting points.

For example, to never flag the words in your own word lists:

```toml
[spell]
word_lists = ["~/.config/nvim/dictionaries/my-field.txt"]
```

Sessions, review state and local history are stored under `~/.local/state/lemma/`. Set
`LEMMA_LOG=lemma_app=info` to turn on logging.

## Development

The workspace is split into GUI-free crates and the GTK app:

| Path | Contents |
|---|---|
| `crates/lemma-app` | The GTK application (`lemma` binary) |
| `crates/lemma-agent` | ACP client, MCP server and the API-key backend |
| `crates/lemma-mcp` | Bridge binary between the agent and the editor's MCP tools |
| `crates/lemma-review` | Hunk model for agent-edit review |
| `crates/lemma-syntax` | LaTeX scanner and project structure index |
| `crates/lemma-complete` | Completion engine and generated package tables |
| `crates/lemma-build`, `lemma-synctex`, `lemma-pdf`, `lemma-preview` | Builds, log parsing, SyncTeX, PDF rendering, math previews |
| `crates/lemma-snippets`, `lemma-search`, `lemma-config`, `lemma-core` | Snippets, project search, settings, shared types |
| `tools/lemma-fake-agent` | Scripted stand-in agent used by every agent test |
| `xtask` | Generates the completion tables from `data/` |

Run all checks the way CI does. This includes rustfmt, clippy, the generated-table check and
every test, with GUI tests running offscreen under GTK's Broadway backend:

```sh
scripts/ci.sh
```

Tests never contact a model or paid API, and never open windows on your desktop. See
[`docs/testing.md`](docs/testing.md) for details and [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
for the design.

## License

Lemma is free software, released under the [GNU General Public License v3.0 or later](LICENSE).

It bundles data from other projects:

- `data/raw/cwl/*.cwl` are completion word lists from
  [TeXstudio](https://github.com/texstudio-org/texstudio) (GPL-3.0).
- `data/raw/umath_map.tsv` is derived from `unicode-math-table.tex` from the
  [unicode-math](https://github.com/latex3/unicode-math) package (LPPL 1.3c).
