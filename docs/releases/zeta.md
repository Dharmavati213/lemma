Lemma is a LaTeX editor for Linux, macOS and Windows (GTK 4 / libadwaita) built around reviewing
an AI agent's edits: every change the agent makes shows up inline as a red/green hunk that you
accept or reject on its own, like a code review. It is also a complete LaTeX editor without the
agent.

**zeta** is Lemma's second release (version 0.2.0) and the first for Windows. It adds everyday
editing commands and recovery of unsaved work, and is much faster with large documents, large
projects and large reviews. All changes are in the [changelog](https://github.com/dharmavati213/lemma/blob/zeta/CHANGELOG.md).

## Highlights

- **New: Windows** 10 (1903 or later) and 11 on x86_64, with an installer and a portable zip.
- **Unsaved changes survive** a crash, a shutdown, a closed terminal or a logout. When the
  document opens again, Lemma offers the recovered text as changes to review or discard.
- **Find and replace in the file** (Ctrl+F, Ctrl+H, F3), **Go to Line** (Ctrl+G), **New File**
  (Ctrl+N), **Save As** (Ctrl+Shift+S), **Toggle Comment** (Ctrl+/), Bold and Italic (Ctrl+B,
  Ctrl+I), F4 and Shift+F4 for the next and previous build problem, and Clean Build Files and
  Rebuild.
- **Start-up**: several files on the command line open in tabs (`lemma *.tex`, and
  `chapter.tex:250` for a line). The window comes back at its last size. A restored session
  opens its tabs in the background. Auto-save is now a preference.
- **PDF**: Fit Page and Zoom to 100% (Ctrl+1), sideways scrolling, Open PDF in Another App and
  Show PDF in Files.
- **Your own packages**: macros and theorem environments from `.sty` and `.cls` files next to
  the document reach completion, hover, go to definition and the Outline. Changes that other
  programs make to the project's files (a re-exported bibliography, a `git checkout`) reach
  completion, the undefined-reference check and the Outline without a build. This includes
  files outside the project's folder.
- **More errors explained** in plain English: TeX capacity exceeded, Dimension too large,
  unknown keyval and pgfkeys keys, undefined xcolor colours, lost floats and bad `\vbox`es.

## Faster

Measured in release builds:

- **Opening a LaTeX file** no longer parses GtkSourceView's LaTeX definitions again each time: a
  400-line chapter opens in 14 ms instead of 44 ms, and a session of 20 such tabs restores in
  0.3 s instead of 1.1 s.
- **Large reviews** (an agent's replace_all, Replace in Project, Format Document): with 3000
  pending changes, accepting or rejecting one takes 20 ms instead of 0.43 s. Accept
  all of 5000 changes takes 0.1 s instead of 0.7 s, and Reject all with its undo no longer
  freezes the window. Replace All with 20 000 matches in an open 10 000-line file takes
  0.45 s instead of 5.5 s.
- **Vim's `:%s`** replaces everything in one edit: 3300 matches in 10 000 lines take 0.27 s
  instead of 18–50 s.
- **Big projects**: the undefined-reference check and the Outline redo only what changed. A
  pause in typing prose, which cost 22–34 ms in a 300-chapter book, now costs under 0.1 ms.
  The Problems page builds only the rows on screen. The Changes panel is no longer rebuilt
  while hidden.
- **The PDF**: dark pages render about twice as fast, with the same pixels. Large pages render
  in full-width bands, and zooming out renders at the new zoom at once. The first SyncTeX
  query after a build no longer holds page rendering back. A reload no longer reads every
  page's size before showing the new PDF.
- **Typing**: `\end{` completion, the math hover and live preview, inline equation numbers, the
  live math card and the review overlay no longer do whole-document work on each key.

## Downloads

| System | File | Install |
|---|---|---|
| macOS 15+ (Apple silicon) | `Lemma-zeta-macos-arm64.dmg` | Open it and drag Lemma to Applications (first start: see below) |
| Windows 10 1903+ and 11 (x86_64) | `Lemma-zeta-windows-x86_64-setup.exe` | Run it (SmartScreen: see below) |
| Windows, portable | `Lemma-zeta-windows-x86_64.zip` | Unzip anywhere and start `Lemma.exe` |
| Ubuntu 26.04+, Debian sid | `lemma_0.2.0-1_amd64.deb` | `sudo apt install ./lemma_0.2.0-1_amd64.deb` |
| Fedora 44+ | `lemma-0.2.0-1.fc44.x86_64.rpm` | `sudo dnf install ./lemma-0.2.0-1.fc44.x86_64.rpm` |
| Arch Linux | `lemma-0.2.0-1-x86_64.pkg.tar.zst` | `sudo pacman -U ./lemma-0.2.0-1-x86_64.pkg.tar.zst` |
| Other Linux distributions | `lemma-zeta-linux-x86_64.tar.gz` | `tar xf lemma-zeta-linux-x86_64.tar.gz && lemma-zeta-linux-x86_64/install.sh` |
| Source | *Source code* (zip, tar.gz) | See the README, "From source" |

**macOS in one command** (installs the app, a `lemma` command and TeX):

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/dharmavati213/lemma/main/packaging/macos/install.sh)"
```

The macOS app is not notarized by Apple, so the first start is blocked: open **System Settings
→ Privacy & Security** and click **Open Anyway** (or run
`xattr -dr com.apple.quarantine /Applications/Lemma.app`). The one-command installer needs no
such step.

**Windows**: the installer is not code-signed, so SmartScreen may say "Windows protected your
PC": click **More info**, then **Run anyway**. To build documents, install TeX: MiKTeX (with
Perl, which its `latexmk` needs) or TeX Live. See the README, "TeX on Windows".

**Linux** packages use the system's GTK 4.22+, libadwaita 1.9+, GtkSourceView 5.18+, libspelling
and poppler-glib 26.01+, the libraries of GNOME 50 and later. Debian 13, Ubuntu 25.10, Fedora 43
and older releases do not have them yet. The tarball's `install.sh` checks for them and names the
packages to install. It installs into `~/.local` (or `/usr/local` with `sudo … --system`), and
`--uninstall` removes Lemma again.

**Verify a download** against `SHA256SUMS`:

```sh
sha256sum -c SHA256SUMS --ignore-missing          # Linux
shasum -a 256 -c SHA256SUMS --ignore-missing      # macOS
```

## Requirements

- A TeX distribution with `latexmk` to build documents: TeX Live (the Linux packages recommend
  it), MacTeX on macOS (the one-command installer sets it up), MiKTeX or TeX Live on Windows.
- Optional: `chktex` (lint), `texcount` (word count), `latexindent` (formatting),
  `mylatexformat` (faster math previews), `git` (the git gutter).
- The Claude, Codex and Pi agents need Node.js and npm; the other agents their own command line
  tool. See the README, "Setting up the agent".

## Known limitations

- Windows support is new and so far tested only under Wine, not on real Windows hardware.
  Please report what does not work, especially with the agents, MiKTeX and fonts.
- macOS: Apple silicon and macOS 15 or later only; not notarized; shortcuts use Ctrl, not Cmd.
- Linux packages are for x86_64.

The Linux packages and the Windows files were built on the developer's machine from this
release's commit: each Linux package in a container of its distribution (Ubuntu 26.04 for the
.deb and the tarball, Fedora 44, Arch Linux), then installed in a fresh container of its
distribution (the tarball on Debian sid), where Lemma opened a document. The Windows app was
cross-built and started under Wine. The macOS app was built by GitHub Actions from the same
commit and started with Homebrew moved aside.
