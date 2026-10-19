# Lemma

Lemma is a LaTeX editor for Linux (GTK 4 / libadwaita) built around reviewing an AI agent's
edits. When the agent changes your document, every change shows up inline as a red/green hunk
that you accept or reject on its own, like a code review, before anything is final.

It is also a complete LaTeX editor without the agent: continuous PDF preview with SyncTeX,
build diagnostics in plain English, completion, snippets, a project outline and more.

## Features

- **Agent edits as reviewable hunks.** Claude Code (over ACP), the Anthropic API with your own
  key, or the Pi agent edits the project through Lemma's own tools. Each edit becomes a hunk
  with word-level highlighting. Accept or reject per hunk (Alt+Y / Alt+N), per file or per
  turn, and switch to a reading mode that hides deletions (Alt+D). Ctrl+K edits just the
  selection or paragraph.
- **Attach images and files to a prompt.** Click the paperclip under the agent's prompt box, drop
  files or images on the agent pane, or paste (Ctrl+V) a screenshot or files copied in a file
  manager. Images (PNG, JPEG, GIF, WebP, and anything else GTK can read; files up to 64 MB) go
  to the model as images, scaled down when they are too large; PDFs and text files are attached
  too. Claude Code and Pi take PDFs and text files of any size: large ones are sent as a link
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
count), `mylatexformat` (faster math previews) and `git` (the git gutter). The agent needs
`node` and `npm`. For the Claude backend you also need the `claude` CLI, logged in.

Arch Linux ships all of these. Other distributions may not have libraries this new yet.

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

Agent edits always arrive as hunks for review. Nothing is accepted for you.

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
