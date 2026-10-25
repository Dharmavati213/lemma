# Changelog

Releases are named after Greek letters; the version number is in `Cargo.toml`. Each release's
notes, with its downloads, are in [`docs/releases/`](docs/releases/) and on the
[releases page](https://github.com/dharmavati213/lemma/releases).

## Unreleased

### Added

- Find and replace in the current file: Ctrl+F opens a find bar above the editor (Ctrl+H with
  its replace row) that highlights every match and counts them ("3 of 17"), with Match Case,
  Whole Word and Regular Expression, F3 / Shift+F3 (or Enter / Shift+Enter) for the next and
  previous match, Replace and Replace All (one undo step). With Vim on, Ctrl+F and Ctrl+H stay
  Vim's keys; in the PDF, Ctrl+F still searches the PDF.
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
- Up and Down in the agent's prompt box bring back the prompts you sent from it in this window
  (the last 50), as in a shell: Up in the empty box shows the last one, Down past the newest
  empties the box again. In a prompt of several lines they move between its lines first, and
  once you edit a prompt they only move the cursor. Listed in the shortcuts window.

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
- The API backend appends each streamed piece of an answer or of its thinking in place instead
  of copying everything received so far, which made a long answer cost time quadratic in its
  length on the agent's thread (32,000 pieces: under 1 ms instead of 230 ms).
- Of the last 64 tool calls, the editor keeps the first 2 KB of each reply instead of the whole
  reply (often a whole file), and forgets them when the agent stops.
- A table in an agent's answer is no longer built again on every frame while it streams: only
  the cells that changed are updated and new rows are added. With a 60-row table, each update
  kept the main thread busy for about 25 ms (over 50 ms at times), which made typing and
  scrolling stutter while the answer streamed; it now takes about 0.5 ms. A selection in the
  table is no longer lost while it streams.

### Fixed

- With an input method that composes text (Japanese, Chinese or Korean input through IBus,
  Fcitx, macOS or Windows; dead keys), Enter in the agent's prompt box confirms the conversion
  instead of sending the prompt without the text being composed. Up, Down, Tab and Escape go to
  the input method too while it composes, and a permission card no longer takes the focus from
  the empty prompt box then (the next keys of the conversion, such as `n`, answered it).
- A table in an agent's answer no longer makes GTK warn ("Trying to measure GtkGrid …") each
  time it is laid out.
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
- `[agent] idle_stop_min` was accepted but did nothing. Now an ACP agent idle for that many
  minutes (10; 0 = never) is stopped to free its memory (about 360 MB for Claude Code), unless
  the agent pane is open in the focused window, its prompt box has text or a permission
  request waits; a note in the transcript says so. The next message (or opening the pane)
  starts it again, and an agent that can resume conversations (ACP `session/load`, as Claude
  Code's adapter can) continues this one, with the mode, model and effort it had, without
  showing the conversation again. One that cannot is only stopped before the first message.
- An ACP agent other than Claude Code (Pi, Codex, Gemini CLI, …) that answered the first
  prompt with an error (a quota, an expired login) went without Lemma's instructions for the
  rest of the session; the next prompt now carries them.

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
