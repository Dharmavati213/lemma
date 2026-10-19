Lemma is a LaTeX editor for Linux and macOS (GTK 4 / libadwaita) built around reviewing an AI
agent's edits: every change the agent makes shows up inline as a red/green hunk that you accept
or reject on its own, like a code review. It is also a complete LaTeX editor without the agent.

**epsilon** is Lemma's first release (version 0.1.0), and the first with macOS support.

## Highlights

- **Agent edits as reviewable hunks.** Claude Code, Codex, Grok Build, Gemini CLI, OpenCode,
  GitHub Copilot, Pi, any other ACP agent, or the Anthropic API with your own key. Accept or
  reject per hunk (Alt+Y / Alt+N), per file or per turn; Ctrl+K edits just the selection.
- **Attach images and files to a prompt**: the paperclip, drag and drop, or paste.
- **Build and preview**: `latexmk` on save, a PDF pane that reloads in place, dark mode, search,
  thumbnails, bookmarks and SyncTeX both ways.
- **Diagnostics in plain English**, with quick fixes for common errors and optional `chktex` lint.
- **Writing help**: completion for commands, environments, labels, citations and Unicode math,
  snippets and autosnippets, spell checking, hover previews of `\ref`, `\cite` and rendered math,
  label rename and references.
- **Navigation**: project outline, command palette, quick open, project search and replace, go
  to definition, session restore, local history and a git gutter. Optional Vim mode.
- **New: macOS** on Apple silicon, with a self-contained app, the API key in the Keychain,
  Finder's *Open With*, and a one-command installer that sets up TeX as well.

## Downloads

| System | File | Install |
|---|---|---|
| macOS 15+ (Apple silicon) | `Lemma-epsilon-macos-arm64.dmg` | Open it and drag Lemma to Applications (first start: see below) |
| Ubuntu 26.04+, Debian sid | `lemma_0.1.0-1_amd64.deb` | `sudo apt install ./lemma_0.1.0-1_amd64.deb` |
| Fedora 44+ | `lemma-0.1.0-1.fc44.x86_64.rpm` | `sudo dnf install ./lemma-0.1.0-1.fc44.x86_64.rpm` |
| Arch Linux | `lemma-0.1.0-2-x86_64.pkg.tar.zst` | `sudo pacman -U ./lemma-0.1.0-2-x86_64.pkg.tar.zst` |
| Other Linux distributions | `lemma-epsilon-linux-x86_64.tar.gz` | `tar xf lemma-epsilon-linux-x86_64.tar.gz && lemma-epsilon-linux-x86_64/install.sh` |
| Source | *Source code* (zip, tar.gz) | See the README, "From source" |

**macOS in one command** (installs the app, a `lemma` command and TeX):

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/dharmavati213/lemma/main/packaging/macos/install.sh)"
```

The macOS app is not notarized by Apple, so the first start is blocked: open **System Settings
→ Privacy & Security** and click **Open Anyway** (or run
`xattr -dr com.apple.quarantine /Applications/Lemma.app`). The one-command installer needs no
such step.

**Linux** packages use the system's GTK 4.22+, libadwaita 1.9+, GtkSourceView 5.18+, libspelling
and poppler-glib 26.01+, the libraries of GNOME 50 and later. Debian 13, Ubuntu 25.10, Fedora 43
and older releases do not have them yet. The tarball's `install.sh` checks for them and names the
packages to install; it installs into `~/.local` (or `/usr/local` with `sudo … --system`) and
removes Lemma again with `--uninstall`.

**Verify a download** against `SHA256SUMS`:

```sh
sha256sum -c SHA256SUMS --ignore-missing          # Linux
shasum -a 256 -c SHA256SUMS --ignore-missing      # macOS
```

## Requirements

- A TeX distribution with `latexmk` to build documents: TeX Live (the packages recommend it),
  MacTeX on macOS (the one-command installer sets it up).
- Optional: `chktex` (lint), `texcount` (word count), `latexindent` (formatting),
  `mylatexformat` (faster math previews), `git` (the git gutter).
- The Claude, Codex and Pi agents need Node.js and npm; the other agents their own command line
  tool. See the README, "Setting up the agent".

## Known limitations

- macOS: Apple silicon and macOS 15 or later only; not notarized; shortcuts use Ctrl, not Cmd.
  macOS support is new: please report what does not work.
- Linux packages are for x86_64.

Every file below was built by GitHub Actions from this tag. Each Linux package was installed in a
fresh container of its distribution and the macOS app was started with Homebrew moved aside;
Lemma opened a document in each.
