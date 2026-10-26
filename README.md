# Lemma

Lemma is a LaTeX editor for Linux, macOS and Windows (GTK 4 / libadwaita) built around reviewing an AI agent's
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
- **Link files to the agent.** The pin button beside the paperclip keeps files and folders
  linked to the agent in a project, such as notes, a style guide or the papers you cite. The agent
  gets them with the first message of every conversation and again whenever they change, so it
  always has them; a folder stands for the PDFs and text files in it.
- **Build and preview.** Saving runs `latexmk` (F5 builds by hand). The PDF pane reloads in
  place when a build writes a new PDF, keeping your place even with a search open. It supports
  dark mode, search, thumbnails and bookmarks, and SyncTeX works both ways: the **Source** and
  **PDF** buttons on the left edge of the PDF, next to the editor, show the page on screen in the
  source and the cursor's line in the PDF (Ctrl+Alt+J, or Show in PDF in the editor's right-click
  menu), and Ctrl+click in the PDF opens the line of the point clicked. Its menu fits the width
  or the whole page (for slides) and zooms to 100% (Ctrl+0 and Ctrl+1 in the PDF; Left and
  Right, or h and l, scroll sideways). It also opens the PDF in another app, to print it or
  present it, and shows it in your file manager, as the build keeps it in a hidden `.build`
  folder. When a damaged `.aux` or `.bbl` keeps a build failing, **Clean Build Files and
  Rebuild** in the command palette runs `latexmk -c` (the PDF stays) and builds again.
- **Diagnostics.** Errors, warnings and bad boxes from the TeX log and `.blg`, with
  explanations and quick fixes for common errors, plus optional `chktex` lint. F4 and Shift+F4
  (`]e` and `[e` in Vim) go to the next and previous build problem, across files.
- **Writing help.** Completion for commands, environments, labels, citations and Unicode math
  glyphs from per-package word lists, and for your own macros, also those of a `.sty` or `.cls`
  next to the document. Snippets and autosnippets, bracket pairing, spell
  checking, hover previews of `\ref`, `\cite` and rendered math, and label rename/references.
  Ctrl+/ comments lines out and back in; Ctrl+B and Ctrl+I put the selection in `\textbf{…}`
  and `\textit{…}` (`\mathbf`, `\mathit` in math) and take it out again, and Emphasize
  (`\emph`) is in the command palette. In Vim mode, Ctrl+B and Ctrl+I stay Vim's keys.
  Optionally, `...` typed in text becomes `\dots`.
- **Navigation.** Project outline, command palette (Ctrl+Shift+P), quick open (Ctrl+P), go to
  line (Ctrl+G, or `:42` and `main.tex:42` in quick open), find and replace in the current file
  (Ctrl+F, Ctrl+H; F3 and Shift+F3 for the next and previous match), project search and replace
  (Ctrl+Shift+F), go to definition (F12), session restore, local history and a git gutter.
- **Optional Vim mode** (Ctrl+Alt+V), off by default. Vim keeps its own Ctrl+F, Ctrl+H, Ctrl+G
  and Ctrl+N; search with `/` and go to a line with `:42`. `:w file` writes a copy, `:sav file`
  saves under a new name and `:e file` opens a file. `:[range]s/pattern/replacement/[gi]`
  substitutes with PCRE patterns (groups as `(…)`) and GRegex replacements (`\1`, `\U`), as
  GtkSourceView's Vim does, in one undo step.

## Requirements

Lemma targets current GNOME 50-era libraries:

| Dependency | Version |
|---|---|
| GTK | 4.22 |
| libadwaita | 1.9 |
| GtkSourceView | 5.18 |
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
macOS, Homebrew has them all (see [macOS](#macos) below). On Windows, MSYS2 has them all, and
the installer and the zip bring them along (see [Windows](#windows) below).

## Installation

### Linux: release packages

Each [release](https://github.com/dharmavati213/lemma/releases) has ready-made packages for
x86_64. Download the one for your distribution and install it:

| Distribution | File | Install |
|---|---|---|
| Ubuntu 26.04 or later, Debian sid | `lemma_<version>-1_amd64.deb` | `sudo apt install ./lemma_*_amd64.deb` |
| Fedora 44 or later | `lemma-<version>-1.fc44.x86_64.rpm` | `sudo dnf install ./lemma-*.x86_64.rpm` |
| Arch Linux | `lemma-<version>-<rel>-x86_64.pkg.tar.zst` | `sudo pacman -U ./lemma-*-x86_64.pkg.tar.zst` |
| Others (openSUSE Tumbleweed, …) | `lemma-<release>-linux-x86_64.tar.gz` | see below |

The packages bring in the libraries they need; TeX (`latexmk`) and a spelling dictionary are
recommended alongside. Older releases (Debian 13, Ubuntu 25.10, Fedora 43 and earlier) do not
have GNOME 50's libraries, which Lemma needs.

For other distributions, unpack the tarball and run its installer. It checks for the libraries
first and tells you which packages to install if they are missing:

```sh
tar xf lemma-*-linux-x86_64.tar.gz
lemma-*-linux-x86_64/install.sh                   # for you, into ~/.local
sudo lemma-*-linux-x86_64/install.sh --system     # or for everyone, into /usr/local
```

`install.sh --uninstall` (with `--system` if you used it) removes Lemma again. Each release also
has `SHA256SUMS` to check the downloads: `sha256sum -c SHA256SUMS --ignore-missing`.

### Arch Linux: build the package yourself

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

### Windows

Lemma runs on Windows 10 (version 1903 or later) and Windows 11, on 64-bit Intel and AMD
processors. Windows support is new, so please report what does not work. There are three ways to
install it.

#### Option 1: the installer

1. Download `Lemma-…-windows-x86_64-setup.exe` from the
   [releases](https://github.com/dharmavati213/lemma/releases).
2. Run it. The installer is not code-signed, so Windows SmartScreen may say "Windows protected
   your PC": click **More info**, then **Run anyway**.
3. It installs Lemma for you alone, without administrator rights (or, if you choose, for
   everyone in Program Files) and adds it to the Start menu. Optional: a desktop icon, Lemma
   under **Open with** for `.tex` and `.bib` files, and the `lemma` command in terminals.
4. Install TeX to build documents (see [TeX on Windows](#tex-on-windows) below).

To open `.tex` files with Lemma by double-clicking them, right-click one, choose **Open with →
Choose another app**, pick Lemma and tick **Always**. Uninstall Lemma in **Settings → Apps**;
your settings stay.

#### Option 2: the zip

Unzip `Lemma-…-windows-x86_64.zip` anywhere (your Documents, a USB stick) and start
`Lemma\bin\lemma.exe`. Nothing is installed; delete the folder to remove Lemma.

#### Option 3: build it yourself

`packaging/windows/build.sh` builds Lemma and makes the folder, the zip and the installer, either
on Windows in [MSYS2](https://www.msys2.org)'s CLANG64 shell or on Linux as a cross build (it
downloads what it needs once). See [`packaging/windows/README.md`](packaging/windows/README.md).

#### TeX on Windows

Either distribution works; Lemma finds both in their usual folders even when they are not on
PATH:

- [MiKTeX](https://miktex.org/download) installs missing LaTeX packages when a document needs
  them; in **MiKTeX Console → Settings**, set that to happen always, so a build is never held
  up by a question. MiKTeX's `latexmk` needs Perl: install
  [Strawberry Perl](https://strawberryperl.com) as well.
- [TeX Live](https://tug.org/texlive/windows.html) (`install-tl-windows.exe`) has everything
  Lemma uses, Perl included.

#### Using Lemma on Windows

- Settings are in `%APPDATA%\lemma\config.toml`; sessions, review state, local history and the
  copies of unsaved changes in `%LOCALAPPDATA%\lemma\state`.
- From a terminal: `lemma path\to\main.tex` (with the installer's PATH option), or run
  `lemma.exe` from the app's `bin` folder with the options under [Usage](#usage); `--help`
  prints in the terminal.
- The API key is stored in the Windows Credential Manager.
- Claude Code: install it with its installer (`irm https://claude.ai/install.ps1 | iex` in
  PowerShell) and [Node.js](https://nodejs.org) for the ACP adapter, which the agent pane's
  **Install** button sets up. The other agents are found as on Linux, also as npm's `.cmd`
  launchers, but have seen less testing on Windows.
- English spell checking is included; Windows' own spell checker covers the languages installed
  in Windows (**Settings → Time & language → Language & region**).
- The git gutter needs [Git for Windows](https://git-scm.com/download/win).

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
lemma main.tex ch/*.tex    # open several documents in tabs; the first is shown
lemma +120 main.tex        # open a document with the cursor on line 120 (or main.tex:120)
lemma path/to/project/     # open a project directory
lemma                      # reopen the file you had open last (with its project's tabs)
lemma .                    # open the current directory as the project
```

The first FILE decides the project, and only the first can be a directory (`lemma . *.tex`).
`FILE:LINE:COLUMN` puts the cursor on a character of the line. The first `lemma` without a file
(or after turning `[session] restore` off) shows an empty window; open a file with Ctrl+O, or
make a new one with Ctrl+N.

Lemma reopens a project's tabs, panes and PDF place, and the window at the size you left it
(maximized, if it was). Unsaved changes survive a crash, a shutdown or a logout: Lemma keeps a
copy of each edited document a few seconds after you stop typing, and when the document next
opens it offers the changes for review as hunks (`[editor] recovery = false` turns this off).

Saving builds the document; Ctrl+Shift+S saves it under another name. F8 toggles the PDF pane,
F9 the sidebar and F10 the agent pane. Ctrl+? lists every shortcut, and Ctrl+, opens
Preferences.

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

An ACP agent that sits idle for 10 minutes is stopped to free its memory (Claude Code takes
about 360 MB), unless the agent pane is open in the focused window, its prompt box has text or
a permission request waits. Your next message (or opening the pane) starts it again, and it
continues the conversation where it left off, in the mode and with the model you had chosen
(after a switch to another agent or project folder, a note says that a new one starts).
An agent that cannot resume conversations is only stopped before your first message to it.
`[agent] idle_stop_min` sets the minutes; 0 keeps the agent running.

In the pane's prompt box, Enter sends and Shift+Enter starts a new line. Up and Down bring back
the prompts you sent from the box in this window (the last 50), as in a shell; once you edit
one, they move the cursor again. While an input method composes (Japanese, Chinese or Korean
input, dead keys), Enter, Up and Down are its keys.

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
it. All three reload live. Preferences (Ctrl+,) writes the same file and keeps your comments;
among other things it turns on auto-save (`[editor] autosave`: when the window loses the focus,
or two seconds after a change). The built-in defaults (`data/snippets.default.toml`,
`data/prompts.default.toml`) are good starting points.

For example, to never flag the words in your own word lists:

```toml
[spell]
word_lists = ["~/.config/nvim/dictionaries/my-field.txt"]
```

Sessions, review state, local history and the copies of unsaved changes are stored under
`~/.local/state/lemma/`; the sessions of projects that are gone, or were not opened for 180
days, are removed, and so are copies not written for 180 days. Set
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
