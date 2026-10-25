# Changelog

Releases are named after Greek letters; the version number is in `Cargo.toml`. Each release's
notes, with its downloads, are in [`docs/releases/`](docs/releases/) and on the
[releases page](https://github.com/dharmavati213/lemma/releases).

## Unreleased

### Added

- Windows support (Windows 10 1903+ and 11, x86_64): an installer and a portable zip, built
  locally with `packaging/windows/build.sh`, natively in MSYS2 or as a cross build from Linux.
  The API key is kept in the Windows Credential Manager, settings in `%APPDATA%\lemma`; TeX
  Live and MiKTeX are found in their usual folders.
- Clean Build Files and Rebuild (command palette): stops the build, runs `latexmk -c`, which
  keeps the PDF, and builds again, for a build that a damaged `.aux` or `.bbl` keeps failing.
- F4 and Shift+F4 go to the next and previous build problem from the cursor, in the Problems
  page's order and across files, opening them at their current line; `]e` and `[e` in Vim
  normal mode. Both are in the command palette and the shortcuts window.
- Toggle Comment (Ctrl+/): comments out the selected lines, or the cursor's line, and back in,
  as one undo step. Blank lines are left alone; `\%` is not a comment.
- Bold (Ctrl+B), Italic (Ctrl+I) and Emphasize (command palette) put the selection in
  `\textbf{…}`, `\textit{…}` or `\emph{…}` (`\mathbf`, `\mathit` in math), or insert the command
  with the cursor inside; on a selection that already has the command they remove it. With Vim
  mode on, Ctrl+B and Ctrl+I stay Vim's keys and Bold and Italic are off.
- Preferences: auto-save (off, when the window loses the focus, or two seconds after a change),
  session restore, the git gutter, the word count, chktex and typing replacements.

### Changed

- A build's toast replaces the previous build's instead of queueing behind it, and a build that
  succeeds or is stopped withdraws it: repeated saves or the agent's `compile` calls no longer
  leave a train of stale "Build failed" toasts.
- Without latexmk, a build says what to install (TeX Live, MacTeX or MiKTeX, with Perl on
  Windows) and that `[build] latexmk` can point at it: F5 every time, saves and the agent once
  per window; the status bar says "latexmk not found".
- Auto-saving several documents requests one build once they are all written, not one per
  document, which ran latexmk a second time for nothing and wrote the documents twice.
- The Problems page builds its rows only when it is shown, and lists at most 500 of a build's
  problems, the most severe ones (errors, then warnings, then bad boxes), followed by a line
  saying how many more there are. The ones left out keep their gutter marks and are still
  reached by F4. A build with 3000 warnings and bad boxes now keeps the main thread busy for
  about 20 ms instead of 0.35 s while the page is hidden, and 0.13 s instead of 0.7 s while it
  is shown.

### Fixed

- Windows: errors TeX reports in a file of the TeX distribution under a drive
  (`C:/texlive/…/geometry.sty:1005: …`), such as an unknown package option, reach the Problems
  page.
- A build that writes no new PDF (latexmk finds nothing to do, as after saving without changes
  or a repeated `compile` from the agent) no longer reloads the PDF: its pages, thumbnails,
  links and an open search are not redone, and a link preview stays open.
- With the PDF search bar open, a rebuild no longer scrolls the PDF away from where you are
  reading. The current match stays current; only a search you start scrolls to its match.
- The PDF strip no longer renders its thumbnails over and over when the rows on screen need
  more than their share of the cache (HiDPI screens, a small `[pdf] cache_mib`).
- `[typing] replacements` was accepted but did nothing. It now replaces `...` typed in text with
  `\dots` (not in math, comments or verbatim text, and not after `\.`); one undo gives back the
  dots. It stays off by default.

## epsilon (0.1.0), 2026-10-20

The first release.

### Added

- Agent edits as reviewable hunks, accepted or rejected per hunk, file or turn, from Claude Code,
  Codex, Grok Build, Gemini CLI, OpenCode, GitHub Copilot, Pi, any ACP agent or the Anthropic API.
- Images and files attached to a prompt.
- Builds with `latexmk` on save; a PDF pane with SyncTeX both ways, dark mode, search,
  thumbnails and bookmarks.
- Diagnostics from the TeX log and `.blg` with explanations and quick fixes; `chktex` lint.
- Completion, snippets and autosnippets, spell checking, hover previews, label rename and
  references, bracket pairing.
- Project outline, command palette, quick open, project search and replace, go to definition,
  session restore, local history, git gutter, optional Vim mode.
- macOS support (Apple silicon, macOS 15+): `Lemma.app` and a DMG, the API key in the Keychain,
  documents from Finder, and `packaging/macos/install.sh`, which installs Lemma and TeX.
- Packages for Ubuntu 26.04+ and Debian sid (`.deb`), Fedora 44+ (`.rpm`) and Arch Linux, and a
  tarball with an installer for other distributions (`packaging/linux/`).
