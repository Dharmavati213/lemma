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
