# Changelog

Releases are named after Greek letters; the version number is in `Cargo.toml`. Each release's
notes, with its downloads, are in [`docs/releases/`](docs/releases/) and on the
[releases page](https://github.com/dharmavati213/lemma/releases).

## Unreleased

### Added

- A small pill of two labelled buttons in the top left corner of the PDF jumps between the
  source and the PDF. **Source** opens the source line at the middle of the page on screen (the
  page number the toolbar shows), or, while the PDF has not moved since **PDF**, the line that
  showed. **PDF** scrolls the PDF to the cursor's line and flashes it, as Ctrl+Alt+J does, and
  shows the PDF pane if it is hidden. Neither button takes the keyboard focus, so typing goes on
  in the editor. Show in PDF is in the editor's right-click menu as well, Show PDF Page in Source
  is in the command palette, and the buttons work in the PDF's own window. Ctrl+click in the PDF
  still opens the line of the point clicked.
- The Linux packages come with AppStream metadata, so software centres such as GNOME Software and
  KDE Discover show Lemma with its description, and Lemma is offered under Open With for `.bib`
  files as well as `.tex` files.
- When a file that is not UTF-8 cannot hold a character typed into it, the message that the save
  failed offers **Save as UTF-8**: the file is written as UTF-8 from then on, which keeps the
  character. A second message reminds you to change an `inputenc` option that names the old
  encoding.

### Changed

- Builds with pdflatex are faster: the preamble is precompiled once (with `mylatexformat`, after the
  first build, in the background) and reused by every build until the preamble, or a package or file
  it reads, changes. Loading the packages was half of each build: for a 27-page thesis with tikz-cd,
  hyperref and microtype, a build after a small edit went from 1.5 s to 0.9 s of CPU time, with the
  same pages, links, bookmarks and SyncTeX positions. The preamble's warnings still show on the
  Problems page. XeLaTeX, LuaLaTeX, a project with a latexmkrc, and preambles that a format cannot
  keep (`\makeindex`, `\makeglossaries`, `\immediate`, images, shell escape) build as before, and so
  does a project whose format pdflatex cannot load. The precompiled preamble is made by the same
  pdflatex that builds the document, also when `[build] latexmk` names the latexmk of another TeX
  installation, and the precompiled preambles of documents that were renamed or deleted are removed.
  Turn it off with **Precompile the Preamble** in Preferences, or `[build] fast_preamble = false`.
- Each build that writes a PDF copies it next to the main document, so the `main.pdf` beside
  `main.tex` that a file manager, a mail or an upload finds is always the one the preview
  shows. It used to stay in the hidden `.build` folder only, and a `main.pdf` beside the source
  stayed whatever an older build or another program had left there. The copy is made only when
  the PDF changed, and replaces the old one in one step, so a PDF viewer that has it open never
  reads half a file. A copy that fails (a viewer on Windows that locks the file) says so in a
  toast. Turn it off with **PDF Next to the Main Document** in Preferences, or
  `[build] copy_pdf = false`.
- Enter takes the first word proposed in prose too, as it already did for commands, references
  and citations: Enter after `Lor` gives `Lorem`. After a word that is already whole (one the
  spell checker accepts or the project uses, such as `the` with `theorem` proposed), Enter still
  breaks the line, and so does every Enter in prose while spell checking is off. Up and Down
  pick another proposal; Escape closes the popup.
- The banner over files with changes to review names who made them. Changes from Restore from
  History and Add Citation are named after them ("Restore from History · 1 file · 2 hunks") and
  grouped under that name in the Changes panel, where they can be kept or rejected together; Undo
  Last Turn still leaves them alone. When the changes come from several sources (the agent and a
  Replace in Project, say, or another program), the banner says "Changes to review" instead of
  crediting them all to the agent.
- The agent pane keeps the last 100 turns of a long session (fewer when they hold a lot of text):
  older turns leave the pane, and a note at its top says how many. The agent's conversation is not
  affected. A long session no longer makes the pane's memory grow without limit.
- When the agent reads a file, Lemma sends at most 256 KiB at a time and tells the agent where to
  read on. A large file that is not open is no longer loaded whole into memory to send a few of its
  lines, nor read to its end.
- Toggle Comment in a `.bib` file works when biber reads it (biblatex). BibTeX still reads lines
  starting with `%`, so Lemma says so instead. Citation completion no longer offers an entry
  commented out with `%`.
- Completion is lighter on the main thread while you type. The proposals are worked out once per key
  while the popup is open (they were worked out twice), and the file names it proposes are no longer
  worked out again after every change to the document's structure or on every tab switch.
- Format Document and Format Selection are available only in LaTeX and BibTeX documents (greyed out
  in the command palette elsewhere).
- `lemma newdir/` for a folder that does not exist says `newdir/: no such directory` instead of
  opening a new document named `newdir`. Without the `/`, a name that does not exist is still a new
  document.

### Fixed

- On Windows, a save that could not replace the file no longer counts as done. Windows does not
  let a file be replaced while another program has it open (a virus scanner, a viewer, TeX
  reading it, or Lemma itself looking at it after the previous save). Lemma then showed the
  document as saved while the file still held the old text; the next save said that another
  program had changed the file. A save now waits up to a second for the file, and if it still
  cannot be written, it says so and the document stays marked as changed.
- On Windows, settings are read again when a program saves `config.toml`, `snippets.toml` or
  `prompts.toml` by writing a new file in its place, as many editors and tools do. Only a file
  written in place was noticed.
- On Linux and macOS, Ctrl+click in the PDF, the Source and PDF buttons and Show in PDF work with a
  PDF that was built on Windows (a project opened from a shared folder or a download, with its
  `.pdf` and `.synctex.gz`), before the first build here. The source files named in such a SyncTeX
  file, with a drive letter and sometimes backslashes, were not matched to the project's files.
- On Windows, Ctrl+click in the PDF, the Source and PDF buttons and Show in PDF work with a PDF
  that was built on Linux or macOS (a project copied or synchronised with its PDF and
  `.synctex.gz`), before the first build on Windows. The source files named in such a SyncTeX
  file start with `/`, which Lemma did not take for the project's files.
- On Windows, the `CLAUDE.md` that **Create Project Instructions** writes names the project's
  files with `/` (`lit/refs.bib`), as LaTeX and every other system do; it wrote `lit\refs.bib`.
- On Windows, the agent is told file names with `/` (`chapters/intro.tex`), as on every other
  system: in the replies of Lemma's tools (the list of files, the outline, errors and warnings,
  the changes to review), in the report of what you accepted or rejected, in the note of where
  you are in the editor, in quick actions and inline edits, and in the names that `@` proposes
  in the prompt box. It was told `chapters\intro.tex`, and a name copied from there into
  `\input{…}` broke the build, since LaTeX reads `\intro` as a command. The agent may still
  name a file either way, and Lemma's own window shows paths as Windows writes them.
- The tips of slanted and curved arrows in TikZ diagrams (tikz-cd among them) no longer vanish
  when the PDF is zoomed out, below about 250% on a display at scale 1. poppler widens a line
  thinner than a pixel to one pixel, but in the rotated coordinate system that pgf draws such a
  tip in, the width came out too small, down to nothing at 45°. Lemma now finds these strokes
  and draws them a pixel wide like the other thin lines. Below that zoom a page takes about
  twice as long to render (17 ms instead of 9 for a page of a thesis).
- The PDF pane no longer switches to an older PDF after a build. Opening a document looks for
  the PDF of an earlier build; when that search ended after the first build had already shown
  its PDF, it opened the PDF it had found, which could be an old `main.pdf` beside `main.tex`
  instead of the new one in `.build` (and without its SyncTeX file). A PDF opened meanwhile now
  stays.
- Text in the PDF is sharp at every zoom. A page was rendered at the zoom rounded to an eighth
  and then stretched to the zoom shown, slightly blurring some letters and not others (at 148%,
  rendered at 150% and shrunk by 1%). It is now rendered at the zoom shown and drawn pixel for
  pixel.
- With auto-save on, Discard in "Save changes?" (and Vim's `:qa!`) really throws the changes away.
  They used to be saved anyway when the window lost the focus as it closed, or when the auto-save
  delay ran out while the dialog waited. Nothing is saved by itself while the dialog asks; after
  Cancel the save follows.
- Saving from "Save changes?" no longer closes the tab or window over text typed while the file was
  being written: the dialog asks again.
- When saving from "Save changes?", or Save All, is refused because another program changed the
  file, the toast offers Overwrite, as Ctrl+S does.
- Format Document and Format Selection no longer empty the document when latexindent gives back no
  text at all, as a broken installation can without reporting an error. The whole text used to be
  replaced, and saved like that when it had no unsaved changes. Now nothing changes and a message
  says why.
- Undoing the deletion of the last line of an agent's change, or of the whole change (`dd` then `u`
  in Vim, or deleting the line and pressing Ctrl+Z), brings the line back as part of the change
  under review. It used to come back as your own text: the change no longer showed it, and rejecting
  the change left the agent's line in the document.
- Undoing back past a rejection brings the agent's text back as a change under review. It used to
  come back as your own text, no longer offered for review, when the rejection had been undone and
  redone (Ctrl+Z, Ctrl+Shift+Z) before further edits, after Alt+U, after a second rejection nearby,
  or after Ctrl+Z and Ctrl+Shift+Z of later changes.
- After Alt+U had taken back a rejection and you went on editing, Ctrl+Z steps back through the
  Alt+U and then the rejection. It could instead put the rejected text into the document a second
  time, as your own text.
- Typing over a selection that starts in the agent's change and runs to its end or past it, then
  pressing Ctrl+Z, gives back exactly what was there before. The typed text used to stay behind as a
  deleted word in the change under review (rejecting the change put it back into the document), or,
  with Ctrl+Z pressed right away, the agent's text became your own. More generally, Ctrl+Z and
  Ctrl+Shift+Z of your own edits near the agent's changes put them back where they were, in your
  text or in the change, however the review has redrawn the changes since.
- Ctrl+Z and Ctrl+Shift+Z around Reject Turn keep the agent's text under review and your own text
  out of it. After you had deleted text inside or right next to the turn's changes (in an agent's
  replace_all, say), undoing back past the Reject Turn and redoing it could make part of the agent's
  text your own, no longer offered for review, or show a line of yours as the agent's change, which
  Reject would have deleted; undoing after Alt+U had taken back a Reject Turn could do the same.
- After you accept a change, Ctrl+Z and Ctrl+Shift+Z keep the agent's other changes under review.
  Undoing an accepted edit could make the agent's next, still pending edit at that place your own
  text, or show your undo as a deleted line to review. Undoing back to a rejection at that place, or
  redoing an agent edit you had undone, could bring the agent's text back as your own.
- After Alt+U takes back an accept, or a rejection whose change you had accepted, Ctrl+Z and
  Ctrl+Shift+Z keep the agent's text under review. It could become your own, also after closing and
  reopening the tab. Alt+U of an accept that can no longer be taken back changes nothing;
  afterwards, Ctrl+Z of your typing next to a change under review could put your text into the
  change.
- Ctrl+Z of your typing, Backspace or Delete after accepting a change still puts each edit back
  where it went. A rejected change next to it could come back as your own text.
- After an accept that took only part of an agent edit's text (the rest taken out by a later edit of
  the agent's, of yours or of another program), Ctrl+Z and Ctrl+Shift+Z put each piece back where it
  belongs: what the accept took comes back as your own text, the rest as a change to review. Undoing
  everything and redoing it after accepts gives the reviewed text back exactly. Part of the agent's
  text could become your own, or accepted text could be offered for review again.
- Alt+U of Reject Turn, after two of the turn's rejected changes had ended up empty at the same
  place, put the second one's text back where the review no longer knew it, and Ctrl+Z then made
  that text your own.
- Alt+N on a change you had just typed back to the original text recorded a decision that rejected
  nothing, so the next Alt+U took back an older decision of that file instead of the newest one.
- Discarding unsaved changes that only undo the agent's saved edits (Discard when closing the tab or
  the window, `:q!`, `:e!`) made the agent's text, which the file still holds, your own. It is a
  change to review again.
- Rejecting a file the agent created, in a folder that has no trash (a container's volume, some
  network folders, a memory file system), deletes the file and says so; Undo in that message, or
  Alt+U, brings it back for review, and its text is in the local history. The file used to stay on
  disk with the agent's text in it, no longer under review, with a message that it could not be
  moved to the trash.
- Ctrl+Z and Ctrl+Shift+Z of a reload (another program changed a file while changes were under
  review) take the other program's change out of the review and put it back. Part of it used to
  become your own text, Ctrl+Z of your earlier edits no longer recognized them, and Reject Turn
  could put the other program's text back.
- When another program changes an open file whose changes are all decided, Lemma keeps the file's
  review, and Ctrl+Z back past the reload to a rejected change brings it back for review. It used to
  come back as your own text once the agent had edited the file again.
- Two Lemma windows with the same document open and unsaved each keep their own copy of the unsaved
  changes, also when they run in containers or sandboxes of their own (such as Flatpak), where both
  can have the same process number. They shared one copy, so one window's copy replaced the other's,
  and saving in one window deleted the other's copy too; after a crash, the other window's changes
  were lost. Copies left by earlier versions are still offered.
- Pending changes saved for the next session are no longer dropped without a word when the file
  changed in between. If Lemma's local history still has the text the agent left, the changes come
  back, and the other program's change is shown for review with them. Otherwise a toast names the
  file and says why its pending changes could not be restored (changed outside Lemma, deleted,
  unreadable, or with unsaved changes), so the agent's text no longer passes for your own.
- Undo after rejecting a file the agent created now works: the file comes back from the trash with
  its tab and its text, your typing included, as a change to review again. Its text also goes to the
  local history ("Before a reject") before the file is trashed, so typing that was never saved can
  be recovered later too.
- Reject Turn on a change that also held other edits (another program's change on the next line,
  say) used to say the turn had no pending changes, although it took the turn's part back, and
  offered no Undo. It now says what it did ("Rejected turn 1's part of 1 change in 1 file …") and
  Undo brings it back.
- Replace in Project no longer turns a file's Windows (CRLF) line breaks into Unix ones. Stripping
  trailing spaces with the regular expression `\s+$`, or replacing anything up to the end of a line
  (`%.*`, `%[^\n]*`), used to remove the carriage return of every line. In a line that ends with a
  Windows line break, `$` now matches before the line break and a match never takes part of it,
  unless the pattern names `\r` or `\n` other than to leave them out (as `[^\r\n]` does); Search in
  Project shows the same matches.
- Two Lemma windows on the same project no longer lose local history entries when one tidies the
  history while the other saves.
- A program on the same computer that connects to Lemma's tool socket and never authenticates can no
  longer keep the agent from getting Lemma's tools by holding connections open. On Windows, any
  local user could do this. While such a program floods the socket with new connections, Lemma may
  still turn the agent's connection away; the agent's side then tries again for a few seconds.
- Opening a named pipe, a socket or a device (from the command line, Open, Go to File or the
  session) is refused with a message. A pipe used to keep the window waiting forever, and
  `/dev/null` opened as an empty document.
- A git that hangs (a network drive that does not answer, a blocked file in the repository) no
  longer leaves the git gutter waiting forever, nor the review of a file the agent wrote outside its
  tools. Git is stopped after 30 seconds, and the file shows no git marks.
- Pending changes are saved for your next session within two seconds of the agent making them, even
  while you keep typing, and the saved state survives a crash of the whole computer.
- If you reject the agent's changes in a file and close its tab, the agent is still told about the
  reject with its next step.
- Local history is readable only by you. A damaged entry in it (after a full disk or a crash) no
  longer leads Lemma to delete the project's whole history, nor loses the entry recorded after it.
- A link in a table in the agent's answer is opened under the same rules as the answer's other
  links: web and mail links in the browser or the mail program, anything else only as a file of the
  project. It used to be handed to whatever program the system has for that kind of address. Links
  to a project file written as a `file://` address with a line now open that line.
- Lemma never makes git fetch anything. A project that came with a `.git` of its own (from an
  archive or a download) could otherwise make git run a command named in that repository's settings,
  when a file was opened (the git gutter) or when an agent changed a file outside Lemma's tools.
- The agent's tools also refuse names that Windows turns into `latexmkrc` (such as `latexmkrc.`), as
  they already refused `latexmkrc`.
- Hovering a formula no longer crashes Lemma when the document defines or `\let`s a command named by
  one non-ASCII character (`\newcommand\€{…}`, `\let\x\é`). Such a `\let` (`\let\é=\alpha`) now also
  reaches the math hover and the live preview whole: they used to copy only `\let\é`, which in a
  XeLaTeX or LuaLaTeX document swallowed the next definition.
- The outline, labels and citations of a file keep updating while an xparse definition is being
  typed. `\NewDocumentCommand{\x}{O}` before its `{default}`, or an `O` followed by a non-ASCII
  letter, stopped them updating, and the agent's file tools and the build's main-document detection
  lost the file meanwhile.
- Local History no longer gets stuck on its spinner when a preview takes more than a moment.
- A `[build] timeout_s` too large for the clock (such as 9223372036854775807) no longer crashes
  builds and the agent's tools; it is taken as a week.
- Replace in Project no longer adds text after the last line break of a file. Replacing `$` with `;`
  (or `^` with `% `) by regular expression used to leave a stray `;` on a line of its own at the end
  of every file. The Search page no longer lists the empty line after a file's last line break
  either.
- A regular-expression search using the CRLF flag `(?R)` and `$` finds its matches in files with
  Windows line breaks.
- Find in Project: a search for text with `&` or `<` that finds nothing now says so, and with
  Regular Expression on, Ctrl+Shift+F searches for the selected text as it is.
- Searching the PDF goes to the first match at or below the top of the view. When the page on screen
  had its only match above the part shown, the search scrolled back up to it instead of going on to
  the next match below.
- Warnings, overfull boxes and other messages from an included file whose name has a space or a
  parenthesis (`my chapter.tex`, `chapter (1).tex`) are listed under that file on the Problems page.
  They were listed under the main document, at the other file's line number.
- The quick fix that adds a `\usepackage` (or a `\newtheorem`) puts the new line after a
  `\usepackage` or `\documentclass` whose options take several lines. It used to put it inside those
  options, which broke the preamble.
- On Windows, the quick fix for "File not found" writes the suggested file name with `/` (it wrote
  `\`, which LaTeX reads as a command).
- The math preview no longer keeps showing the old version of a macro file the preamble reads when
  it was saved again while the preview's format was being built.
- Delete Environment keeps Windows (CRLF) line breaks. The line of an `\end{…}` that followed text
  lost its `\r`, and deleting an environment at the very end of a file left a stray `\r`.
- Files dropped on the editor are inserted with the document's own line breaks. In a document with
  Windows (CRLF) line breaks, the inserted lines had Unix ones.
- Bold, Italic and Emphasize refuse a selection that starts right after a backslash or ends with
  one, such as `50\` of `50\%`. Wrapping it gave `\textbf{50\}%`, whose brace is escaped, or a line
  break `\\textbf…`. A message says why.
- While an input method is composing text (Chinese, Japanese, Korean, …), Tab is left to the input
  method instead of moving the cursor out of the brackets.
- Go to File (Ctrl+P) opens files whose names are not UTF-8 instead of an empty new file.
- A tool the agent was running when you pressed Stop, or when the agent stopped, no longer spins
  forever in the agent pane. A tool whose end the agent never reported in a turn that finished shows
  as done, not failed.
- Ctrl+K with Codex, Gemini CLI, Pi and the other agents no longer tells them to use a tool only
  Claude Code has. The edit popover no longer closes as if the edit were done when the turn was
  stopped or ran out of tokens; it says what happened.
- The editor's right-click menu names the agent you chose instead of always "Claude".
- An agent that stops unexpectedly after it started is described as such, not as "could not start".
- The note that `config.toml` selects another agent appears once, not again at every change of
  `config.toml`.
- Attaching a named pipe or a device file is refused instead of keeping the attachments, and Send,
  waiting forever. An `@file` mention of a very large file is refused without reading it first. The
  messages that refuse an attachment state the limits as the README does (64 MiB, 20 MiB, 512 KiB).
- Cancelling the adapter's installation also stops what npm started, so Cancel no longer waits for
  it. An installation whose output had a line in another encoding no longer hangs.
- Keep Turn and Reject Turn say that changes shared with other edits are left to review (they said
  "kept"). The Changes page's plain diff shows "No Changes" when nothing is left to review, instead
  of a blank page.
- With your own API key, the cost estimate for Claude Sonnet 5.5 no longer counts cached input at
  twice its price ($0.20 instead of $0.10 per million tokens), so turns that reuse the cache no
  longer show too high a cost.
- With your own API key, a request the API answers with a timeout (408) or a conflict (409) is
  retried like an overloaded one, and the API's own word on whether to retry is followed. A reached
  monthly spend limit is reported at once instead of after three retries.
- An agent that sends a single message over 64 MB no longer makes Lemma hold all of it in memory:
  the message is dropped and a warning is logged. When the message was a request (writing a file
  that large, say), the agent gets an error in answer instead of waiting for one for ever.
- Zooming into a large page no longer shows thin lines between its parts for the moment before it is
  drawn sharp again.
- Moving the PDF into its own window and back no longer leaves part of that window behind each time,
  which also made every scroll of the PDF a little slower afterwards.
- The word count's popover no longer says "Counting words…" for as long as it is open when texcount
  fails on a large project; it says why the words could not be counted.
- The git gutter shows every line of a new, uncommitted file as added, also when the file does not
  end with a line break; it works in a folder whose name ends with a space; and it updates after
  `git reset --soft` on a branch such as `feature/x`.
- Add Citation: closing the dialog while the entry is fetched gives the entry up (it used to be
  added later, with `\cite{…}` inserted wherever the cursor was by then); closing the `.bib` file's
  tab and discarding its changes while the entry is fetched no longer saves those changes with it;
  an entry with an `@` in a field before its DOI is recognised as a duplicate; links such as
  `https://www.doi.org/…` and `https://export.arxiv.org/abs/…` work, and so do IDs with invisible
  characters copied from a web page; a `.bib` with Windows line breaks keeps them; and a redirect
  never leaves HTTPS.
- Preferences can change settings in a `config.toml` that starts with a byte-order mark, as
  Notepad's "UTF-8 with BOM" saves it (changing a setting of the file's first section used to fail
  with a "duplicate key" error), and a `config.toml` with Windows line breaks keeps them.
  Preferences shows a configuration path containing `&` or `<`, and no longer replaces a
  `config.toml` you cannot write (read-only, another user's, or linked into a read-only place) nor
  fails on one in a folder you cannot write; its settings are shown greyed out.
- With typing replacements on, a space or line break typed right after the `\dots` that `...` became
  is now printed: Lemma writes `\dots{}` before it (one Ctrl+Z takes the braces back). `three...
  and` used to print as "three…and". `...` typed before a space or a line break that is already
  there becomes `\dots{}` at once.
- Tab with only spaces before the cursor indents the line, instead of jumping past a closing bracket
  further on.
- Files dropped on the editor and pasted images get paths from the main document's folder, where
  LaTeX looks for them, so they work from a chapter in a subfolder (from the file's own folder under
  `\subfile` or `\import`).
- Align Columns lines up columns that hold CJK characters, emoji or combining accents.
- Search in the PDF finds `Straße` when you search for `strasse`, and the reverse, as it already
  found ligatures such as `ﬁ`. It also finds a word hyphenated across a page break, shown on the
  page where it starts.
- A link or a bookmark to a page turned sideways (`/Rotate 90`, a landscape page) scrolls to its
  target. It used to land at the wrong height, often just past the target.
- Two forward searches in a row (Ctrl+Alt+J twice, or the follow-cursor search and yours) each go to
  their own line, and Source then returns to the line shown. A forward search answered after another
  document's PDF appeared no longer highlights a place in that PDF.
- Back in the PDF (Alt+Left, Backspace, the mouse's back button) no longer jumps to a page of the
  previous PDF after the root document of another tab showed its own PDF.
- Scrolling the PDF with a long outline in the Bookmarks strip no longer costs about 1 ms per scroll
  step for 5000 bookmarks.
- `\verb |…|` with a space before the delimiter, which LaTeX allows (also `\verb*`, `\Verb` and
  `\lstinline`), is no longer read as LaTeX: a `\section` or `\label` inside it no longer shows in
  the outline, and the highlighting treats it as verbatim.
- Errors in a file whose path contains a colon (a folder named `12:30`, say) are reported on the
  Problems page at their file and line. They used to be missing.
- Cancelling "Choose the Main Document" after F5 no longer shows an error.
- Clicking into the editor while Find in File is still counting a new query's matches no longer
  moves the cursor and the view to the first match.
- Search in the project leaves out the build output folder also when `[build] outdir` is written
  `./build` or `build/`.
- An agent that crashes in the middle of a turn no longer sends a desktop notification saying it
  finished. The turn shows as stopped, and the panel says the agent stopped unexpectedly.
- With your own API key, a conversation goes on after a response in which Claude switched to its
  fallback model midway. The history sent with the next message now keeps the switch marker where
  the API requires it, so the next request is no longer rejected.
- An agent other than Claude Code is answered with an error when Lemma cannot serve one of its file
  reads or writes, instead of waiting forever.
- A quick action started while the agent is busy shows one message, not two.
- Add Citation keeps a `.bib` file that is not UTF-8 (Latin-1) in its encoding, and refuses an entry
  with characters that encoding cannot hold (such as the ’ and – of many Crossref entries), saying
  which ones, before changing anything. Such an entry used to be added and then fail to save. An Add
  Citation that fails before `references.bib` exists (a DOI not found, a folder that cannot be
  written) no longer leaves an empty `references.bib` behind.
- Local History says when a snapshot differs only in its line breaks (Windows CRLF against Unix LF),
  instead of showing every line as changed with no visible difference; there is nothing to restore.
- New from Template no longer leaves empty folders behind when writing the template's files fails
  part way.
- The demos, and the math preview of scripted runs, keep their temporary files in a new private
  folder instead of a fixed name in /tmp that another user could create first, and remove it when
  the run ends.
- The last save before a window closes gets its Local History snapshot. A snapshot still waiting to
  be written when the window closed was dropped, so the version you closed with was missing from the
  history.
- Completion of file names in a document read through `\subfile` or `\import` proposes the paths
  that work there (`../img/plot.png` in a subfile, `fig.png` below `\subimport{parts/}`), not the
  paths from the main document's folder.
- An agent's request for the whole text of a file larger than 64 MiB that is not open in the editor
  is refused with a message, instead of loading the file into memory.
- Closing a window or quitting right after a build started (saving and pressing Ctrl+Q at once) no
  longer leaves latexmk running after the window or Lemma has closed.
- When latexmk cannot run a program because it is not installed (biber, or the TeX engine such as
  lualatex or pdflatex), the Problems page names it: "biber was not found: install it with your TeX
  distribution". It used to say only "latexmk failed with exit code 12", over latexmk's general
  advice. Other latexmk failures now show the last lines latexmk printed before that advice.
- A build stopped by its time limit says the limit it ran with. After `[build] timeout_s` was
  changed during the build, the message gave the new value.
- The Outline no longer shows the section numbers of another main document with the same file name
  (`paper/main.tex` and `slides/main.tex`) after that one was built.
- Saving a document that is not in UTF-8 (a Latin-1 file) with characters its encoding cannot hold
  now names them: "the file's encoding, ISO-8859-15, cannot hold “→” and “’”". It used to say
  "Invalid byte sequence in conversion input", and the document could not be saved at all until the
  characters were found and taken out; the message now has a **Save as UTF-8** button (see Added).
- After a session could not be saved once (a full disk, a folder that could not be written), the
  next save no longer forgets the place in a PDF that was not shown yet, or a side pane's visibility
  while a narrow window hid it.
- The agent's file tools refuse a named pipe, a socket or a device in the project, as opening one in
  a tab does. Reading or editing one kept the agent waiting for an answer forever, and the turn with
  it.
- Hovering a reference whose target sits at the very right or bottom edge of its page no longer
  crashes Lemma.
- Snippets of several lines in a `snippets.toml` saved with Windows (CRLF) line breaks, as Notepad
  saves it, no longer add an empty line after each of their lines in a document with Windows line
  breaks, nor put Windows line breaks into a document with Unix ones.
- On Windows, a project folder whose path has many non-ASCII letters (Chinese, Cyrillic, accented)
  is no longer taken for one longer than Windows' limit of 260 characters. Each such letter used to
  count two or three times, and the folder's path was then given to TeX and other programs in the
  `\\?\C:\…` form that many of them do not take. A file whose own path is longer than the limit is
  no longer refused as lying outside the project when the agent reads or edits it.
- `lemma --help` no longer prints one line of 135 characters.
- Quitting Lemma while the math preview is compiling a formula no longer leaves TeX running. A
  formula that never finishes (a macro defined in terms of itself) used to keep a pdflatex process
  running at full speed after Lemma had quit.
- In Vim's insert mode, Ctrl+W at the start of a line in a file with Windows (CRLF) line breaks
  joins the line to the one before. It used to delete only half of the line break, leaving the lines
  apart and a stray carriage return in the file. Ctrl+W after a word whose accents are separate
  combining characters deletes the whole word, not just the last accent.
- Go to Line (Ctrl+G), and a line number given to Go to File, put the cursor at the start of a blank
  line in a file with Windows (CRLF) line breaks. The cursor used to land inside the line break, so
  text typed there split it.
- The underline that appears under a reference or file name while Ctrl is held no longer stays on
  the text when the text changes while Ctrl is still down (Ctrl+V, Ctrl+Z), or when underlined text
  is copied and pasted. Copying and pasting a highlighted `\begin`…`\end` pair or bracket no longer
  pastes the highlight along with it either.
- Completion of file names (`\includegraphics{`, `\input{`) no longer proposes files whose names
  LaTeX cannot read: names with `%`, `#`, braces, or (outside Windows) a backslash. Accepting one
  broke the document, and on Linux and macOS a backslash in a name became a folder separator.
- Citations are completed from a bibliography in a folder whose name is not valid UTF-8.
- In a file with a lone carriage return or a Unicode paragraph separator (U+2029) inside a line, for
  example text pasted from another program, a change under review whose band sits at that line no
  longer leaves empty gaps above or below each part of the line, and equation numbers below such a
  character are shown on the right line.
- Switching spell checking off and on, or changing its language, no longer leaves something behind
  in the document each time that made editing slightly slower. A spell-checking word list saved as
  "UTF-8 with BOM" (as Notepad can) no longer leaves its first word marked as misspelled.
- When the system ends Lemma (a logout, `SIGTERM`) while a dialog is open, the window closes and the
  session is saved. Only the dialog used to close, and Lemma stayed until it was killed.
- A desktop notification, the editor's right-click menu and Ctrl+K name the agent that is running,
  also after `config.toml` chose another one for the next session.
- When another program replaces the file of an open document (or one Lemma keeps for a review) with
  a named pipe, Lemma no longer waits forever for it. Saving or reloading the document, opening it
  in a tab again, the conflict bar's Reload and Keep Mine, and the agent's tools refuse the pipe
  with a message, and the document keeps its text. Lemma's notice of changes by other programs used
  to stop working in that window. The agent's outline tool also refuses a named pipe in the project
  instead of never answering.
- `\addbibresource{refs}` reads the bibliography named `refs`, as biber does, and no longer
  `refs.bib`, which biber does not look for (biblatex wants the full name:
  `\addbibresource{refs.bib}`). Citations used to be completed and found from `refs.bib` while the
  build failed; now hovering a citation says that `refs` could not be read. A bibliography really
  named `refs` is read, and changes other programs make to it are followed. `\bibliography{refs}`
  still reads `refs.bib`.
- When the first line of a document is right-to-left text (Hebrew, Arabic), the deleted lines shown
  in a review band start at the left edge of the text and wrap at the editor's width again. They
  were drawn from the right edge, on one line that ran out of the window.
- Adding or removing a math environment in the settings (`[editor] math_envs`) now updates the
  spelling underlines of lines that were not on screen yet. A word on such a line could keep its
  underline (or not get one) until the line was edited.
- The macOS app, the Windows folder and installer, and the Linux `.deb`, `.rpm` and tarball include
  the copyright and permission notice of the SyncTeX parser built into Lemma, as its licence asks
  (`LICENSE.synctex` beside Lemma's licence; in the `.deb`'s `copyright` file).
- Every package also includes the licences of the Rust crates built into Lemma (`LICENSE.crates`
  beside Lemma's licence), whose MIT, Apache and BSD licences ask for their notices to go with the
  program. The macOS app includes the licence files of the Homebrew libraries inside it
  (`Resources/licenses`); the Windows folder already had those of its MSYS2 libraries.
- On Windows, an image or a PDF attached to a message is recognised by its contents. Where Windows
  had no content type registered for the file's extension, it was refused as "not a text file, an
  image or a PDF".
- Closing a tab right after its Ctrl+K box was closed (Esc, or a click elsewhere) no longer leaves
  the box on the closed editor, where GTK could print a warning without end.
- `scripts/perf-wayland.sh` fails a run that ends without printing its measurements (a `lemma` that
  does not know `LEMMA_PERF`, a window that never drew a frame) instead of counting it as within
  budget.
- The Linux tarball's `install.sh` writes a launcher that starts Lemma when the folder it is
  installed into has a space or another special character in its name (a home folder with a space,
  for example).
- The Linux tarball's `install.sh` also installs the licence, as the packages do, and
  `--uninstall` updates the icon cache. `install.sh --help` lists `--force` and no longer ends with
  a line of the script.
- On a Mac, `install.sh` reports a download that broke off as such. It used to go on with the
  partial file and then say that BasicTeX "is not signed", or that the DMG could not be opened.
  `--uninstall` also removes the log of the Perl modules it installed for latexindent.
- Uninstalling Lemma on Windows removes all of its "Open with" registration. The list of file types
  it opens (`.tex`, `.bib`) used to stay behind in the registry.
- `packaging/windows/build.sh`, run in MSYS2 on Windows, no longer stops without a message when it
  gathers the licences, and no longer copies the licences of every MSYS2 package into the folder.
  `--help` no longer prints lines of the script. On Linux it gives Wine the Liberation fonts also
  when they were installed after its first run.

## zeta (0.2.0), 2026-10-25

The second release, and the first for Windows.

### Added

- Go to Line (Ctrl+G) opens quick open with `:` typed: `:42` goes to line 42 of the current
  document and `:42:7` to its column 7 (as the status bar counts them), and `main.tex:42` (or
  `./main.tex:42:`, as TeX writes it) to line 42 of a file; a line past the end is the last one.
  Alt+Left goes back. With Vim on, Ctrl+G stays Vim's; in the PDF's search, it is still the next
  match.
- New File (Ctrl+N) and Save As (Ctrl+Shift+S), in the main menu and the command palette. New
  File asks for a name in the current document's folder and opens the file in a tab; its first
  save creates it. Save As writes the document, in its encoding and with its line breaks, under
  the new name and shows that file in the same tab, the cursor where it was; the old file stays
  as it is on disk, and the undo history starts afresh. Save As is refused while the agent is
  working, or while the document has agent changes that are not decided. With Vim on, Ctrl+N in
  the editor stays Vim's.
- Vim: `:w file` writes a copy and goes on editing the current file (`:w! file` replaces a file
  that exists), `:wq file` writes the copy and closes the tab, and `:sav file` (`:saveas`) is
  Save As, refusing a file that exists unless `:sav!`. `:w other.tex` used to say that writing
  to another file is not supported. `:w !cmd`, `:w >> file` and the `%` and `#` file names are
  not supported (a toast says so).
- Several FILEs on the command line (`lemma *.tex`, `lemma main.tex refs.bib`) open in tabs of
  one window; the first decides the project and is shown. `+LINE FILE`, `FILE:LINE` and
  `FILE:LINE:COLUMN` put the cursor there, as other editors take them. Several `.tex` files
  opened together from a Linux file manager open in one window too (the desktop entry passes
  them all).
- The main window opens at the size it had when it was last closed, and maximized if it was.
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
- Open PDF in Another App and Show PDF in Files, in the PDF toolbar's new menu and the command
  palette: the system's PDF viewer opens the built PDF (to print it or present slides), or the
  file manager shows it, since the build writes it to the hidden `.build` folder.
- Fit Page and Zoom to 100% for the PDF (in the toolbar's menu and the command palette; Ctrl+1
  in the PDF is 100%). Fit Page shows the current page whole and keeps doing so when the pane is
  resized, and the session remembers it.
- In the PDF, Left and Right (and h and l) scroll sideways when zoomed in. The shortcuts window
  lists the PDF's own keys.
- Macros and theorem environments from your own packages and class: with
  `\usepackage{mymacros}` and a `mymacros.sty` next to the document or the main file (and the
  packages it loads in turn), or a `\documentclass` whose `.cls` is next to the main file, their
  commands are completed, shown on hover and found by go to definition (F12), and their
  `\newtheorem`s give the Outline its theorem rows. Internal names with `@` are not offered.
- Plain-English explanations on the Problems page for more errors: TeX capacity exceeded (most
  often a command that calls itself), Dimension too large (TikZ and pgfplots), an unknown
  option key (`\includegraphics[widht=…]`, a TikZ key such as `colr=red`), an undefined color,
  Float(s) lost, Too many unprocessed floats and the output loop that follows it; and for bad
  vertical boxes (Overfull and Underfull `\vbox`), which had none.
- Unsaved changes survive a crash, a shutdown, a closed terminal or a logout. Five seconds after
  you stop typing (at the latest every 30 seconds while you type on), and whenever the window
  loses the focus, Lemma keeps a copy of each edited document's text in
  `~/.local/state/lemma/recovery/`; saving or discarding the changes removes it. When the
  document next opens, a bar says "Unsaved changes to “main.tex” from 5 minutes ago were
  recovered" (and whether the file changed on disk since): Review shows them as hunks of a
  "Recovered changes" turn, to keep or reject one by one, and Discard throws them away. While
  the bar waits for an answer, what you type in that document is copied too, next to the
  recovered text: after another crash, the bar offers both, the newest first (Review of the
  older one waits until the changes Review made from the newer one are kept or rejected, so
  that it cannot take them away unseen). A New File that was never saved gets its tab back
  with the session. On Linux and macOS, `SIGTERM`, `SIGHUP` and Ctrl+C in the terminal no
  longer kill Lemma on the spot: it writes the copies and closes its window without asking (a
  Lemma started with `nohup` still keeps running when its terminal closes).
  `[editor] recovery = false` (or Preferences) turns it off.

### Changed

- A restored session opens its tabs in the background: the focus no longer passes through every
  tab while it restores, and only the tab that is shown loads spell checking and completion at
  start-up, the others when they are first selected.
- Start-up no longer shows "No Document Open" before the document appears, and the sidebar, the
  PDF pane and its strip are as the session left them from the first frame instead of moving
  there once the document is open.
- The sessions of projects whose directory is gone, or that were not opened for 180 days, are
  removed, and so are temporary files a crash left in the sessions directory.
- A build's toast replaces the previous build's instead of queueing behind it, and a build that
  succeeds or is stopped withdraws it: repeated saves or the agent's `compile` calls no longer
  leave a train of stale "Build failed" toasts.
- Without latexmk, a build says what to install (TeX Live, MacTeX or MiKTeX, with Perl on
  Windows) and that `[build] latexmk` can point at it: F5 every time, saves and the agent once
  per window, and again only after a build that ran latexmk; the status bar says "latexmk not
  found".
- Auto-saving several documents requests one build once they are all written, not one per
  document, which ran latexmk a second time for nothing and wrote the documents twice.
- In dark mode the PDF strip's thumbnails (and link previews) are rendered dark by the PDF
  thread instead of being recoloured on every frame: drawing a 120 × 612 strip takes 0.3 ms
  instead of 2.1 ms with the cairo renderer, Lemma's default.
- PDF pages too large for one bitmap (fit-width pages in a pane wider than about 900 px on a
  HiDPI display, or any page zoomed in far enough) are rendered in a few full-width bands instead
  of 512-pixel squares, each of which made poppler go through the whole page: such a page now
  takes 23–29 ms to render instead of 77–93 ms (10–12 ms as one bitmap). Zoomed in so far that
  a page is over 4096 pixels wide, it is rendered in squares of about 1024 pixels instead: a view
  of it takes 31–34 ms instead of 77–78 ms. The band just below the view, on the same page or at
  the top of the next, is rendered ahead, so scrolling down finds it ready; these pages were never
  rendered ahead before.
- Dark PDF pages are coloured about five times as fast, to the same pixels: a dark page at fit
  width on a HiDPI screen renders in about 8–9 ms instead of 17–22 ms (6–7 ms light), and at
  scale 1 a dark page costs no more than a light one. Dark pages are the default with a dark
  system style.
- The first SyncTeX search after a build (Ctrl+click, Ctrl+Alt+J or follow-cursor) no longer
  holds up the PDF while the SyncTeX file is read, which takes about 130 ms for a 500-page
  document: it is read in the background and the pages appear meanwhile (after 14–25 ms instead
  of 170–230 ms when the click comes as the PDF reloads). If you used SyncTeX since the build
  before, or in the last five minutes, a build's SyncTeX file is read as soon as its PDF
  reloads, so the search is answered at once (under 1 ms instead of 130–190 ms); otherwise it is
  never read ahead. When the window has been in the background for a minute, or the PDF pane is
  hidden, the SyncTeX data is freed (47 MiB for 500 pages). A PDF in a window of its own that
  you are reading keeps it, and the pages it rendered ahead, while the main window is in the
  background.
- A rebuilt PDF with as many pages as before shows without reading every page's size first: on
  the PDF thread a 500-page document shows after 6 ms instead of 24 ms (about 0.6 s sooner for
  6000 pages). The sizes are read afterwards, and a page whose size changed is laid out again
  with the view keeping its place.
- With the PDF strip on Bookmarks, a build that leaves the outline's titles as they were updates
  the rows where the entries now point instead of building every row again: with 500 bookmarks
  the window no longer stalls for about 60 ms after each build (about 1 ms now). The expanded
  entries and the selected row stay as they were.
- Zooming the PDF out renders the pages that come into view at the new zoom right away, instead
  of at the old zoom and then again 120 ms later (over several Ctrl+wheel notches or a pinch,
  such a page then waits for the final zoom, and a page in bands gets the bands it lacks at the
  scale of its first ones): zooming pdf50 out sixfold now renders 9 pages instead of 14, and the
  texture cache holds 7.8 MiB instead of 13 MiB meanwhile. Until the final zoom, the band below
  the view is not rendered ahead at the old zoom either, and a page in bands or tiles that is
  one bitmap at the new zoom and shows more than they cover is rendered whole at the new zoom at
  once: Ctrl+1 from 700% renders 4 pages, where it rendered 18 tiles at 700% (65 MiB) and then
  the same 4 pages.
- The Problems page builds its rows only when it is shown, and lists at most 500 of a build's
  problems, the most severe ones (errors, then warnings, then bad boxes), followed by a line
  saying how many more there are. The ones left out keep their gutter marks and are still
  reached by F4. A build with 3000 warnings and bad boxes now keeps the main thread busy for
  about 20 ms instead of 0.35 s while the page is hidden, and 0.13 s instead of 0.7 s while it
  is shown.
- The Problems page is one list that makes widgets only for the rows near the ones on screen
  (about 200) and replaces only the files whose rows changed, where it made a row of widgets for
  each of up to 500 build problems and 500 findings of each section, and made them all again
  for any change. With the page shown, a build with 3000 warnings and bad boxes now costs the
  page about half the main thread's time it took with those 500 rows, with no step over 120 ms
  instead of 250 ms, and the PDF is asked for before the page lists the problems.
- Undefined references and citations reach the Problems page at once, and only those of the
  files whose names changed, not every file's one file at a time, each of which listed the
  section's rows again. Enter in one of 40 chapters with 400 undefined references in all held
  up the window for 2 to 4 s while the page was shown (about 18 s with 6700 in 300 files); it
  now takes 25 to 50 ms.
- The Changes panel is no longer built again after every decision and every agent write: while
  its page is hidden it waits until it is shown, and shown, a count changes in its row, and only
  the rows of files that came or went are added or removed. With 300 files under review,
  accepting a change costs about 2 ms instead of 0.2 s, and an agent turn that writes all 300
  files takes 1.3 s instead of 41 s.
- A build's log is turned into problems one file at a time instead of one diagnostic at a time:
  24 000 warnings in 300 files take 39 ms instead of 0.7 s (on a busy machine) between
  latexmk's exit and the PDF's reload.
- The API backend appends each streamed piece of an answer or of its thinking in place instead
  of copying everything received so far, which made a long answer cost time quadratic in its
  length on the agent's thread (32,000 pieces: under 1 ms instead of 230 ms).
- F3, Shift+F3 and Enter in the find bar right after a new query, while the search is still
  counting its matches, no longer hold up the window until they find their match (up to 47 ms
  in a 60 000-line file when the only match is near the end or there is none, 8 ms in 10 000
  lines): the match is looked for in the background, and pressing the key several times still
  goes on that many matches.
- Of the last 64 tool calls, the editor keeps the first 2 KB of each reply instead of the whole
  reply (often a whole file), and forgets them when the agent stops.
- Completion's word list is built in one pass over the project's files instead of being sorted
  and copied again for each file (300 files of 400 lines: under 0.2 s instead of about 0.8 s),
  and a build that changes only the `.aux` or the bibliography no longer reads all the
  project's files again for their words.
- The agent's `compile` and `get_diagnostics` and `:Agent fix-this-error` look up each open
  tab's file once, not once for every build problem.
- A table in an agent's answer is no longer built again on every frame while it streams: only
  the cells that changed are updated and new rows are added. With a 60-row table, each update
  kept the main thread busy for about 25 ms (over 50 ms at times), which made typing and
  scrolling stutter while the answer streamed; it now takes about 0.5 ms. A selection in the
  table is no longer lost while it streams.
- The live math preview's card no longer makes GTK lay out the whole document again each time
  it appears, moves or goes away, and neither do the underlines of undefined references, build
  problems and chktex findings when they change, nor deciding a hunk. Each of these removed a
  tag from the whole text, which on a 60k-line document kept the main thread busy for about
  1.2 s afterwards (typing and scrolling stuttered meanwhile); it now takes under 1 ms.
- Typing the name after `\end{` no longer copies the whole document on every key to find the
  environment that is open; it reads only the 256 KiB before the cursor. On a 60k-line
  document the proposals take 0.35 ms instead of 3.5 ms, and 0.75 ms at the top level of the
  body (`\end{document}`), where all of those 256 KiB are looked through.
- The math hover and the live preview read a document's preamble and definitions again only
  after an edit that could change them (in the preamble or a definition, or one that types or
  deletes a backslash, a `%` or a line break), not after every edit: on a 60k-line document,
  the preview after typing other text costs under 0.5 ms instead of 5 ms.
- Inline equation numbers look each label up once: a refresh of a 60k-line document with 5000
  labels takes 8 ms instead of 49 ms. The Unused dialog names its labels the same way.
- Deciding and typing in a file with thousands of hunks (a Format Document, a project-wide
  replace) no longer takes a large fraction of a second each time. The review overlay gave
  every hunk a tag of its own, and every tag in a buffer slows down all of its tag changes; it
  also looked for each hunk in its layout cache one by one, and took the green of the added
  lines and changed words off the whole file and put it back after every decision and after
  typing in a hunk. Hunks that reserve the same space above their line now share a tag (a few
  tags instead of thousands), and the overlay changes the tags of the lines that changed only.
  With 3000 hunks, its update after a recompute takes 10 ms instead of 85 ms, Alt+Y or Alt+N
  20 ms instead of 0.43 s, and the pause after typing in a hunk 18 ms instead of 0.37 s. With
  5000 hunks in an open file, the whole action with the overlay, measured before the review's
  own part of Accept all got faster too (below): Accept all in the file 0.1 s instead of
  0.67 s, Keep Turn 0.1 s instead of 0.64 s, Reject all 0.4 s instead of 1.3 s, Reject Turn
  0.4 s instead of 2.2 s, the agent's edit that makes them 0.56 s instead of 1.1 s, and a
  keystroke 0.25 ms instead of 0.46 ms (p99). While typing moves hunks, the gutter's red
  wedges stay next to their bands until the recompute instead of disappearing.
- With the Outline shown, typing prose no longer makes it recompute the whole outline at every
  pause: a document whose headings, theorems, labels, captions and includes stay as they were
  keeps its place in the project index, and only the words for completion are collected again.
  Each pause cost the main thread about 5 ms in a 60 000-line document and about 29 ms in one
  with 2000 figures; it now costs nothing. Captions are no longer read line by line from the
  editor when the outline is computed.
- After a build, Lemma looks for the packages of the distribution again (they may have become
  local ones) without touching the file system on the main thread: with 40 packages that was 52
  path lookups each time.
- Files under review that are not open take less memory. The review kept a record of each of
  the agent's changes (or Replace in Project's) to such a file, which only an editor's undo
  could use, though a file that opens starts with an empty undo history; it no longer does, and
  closing a tab drops those of its changes. Replace in Project of 600 matches in each of 50
  closed files: 17 MB instead of 30 MB; one file with 20 000: 11.5 MB instead of 20 MB. Alt+U
  still takes back a Reject all or a Reject Turn in a closed file. In an open file, the records
  that let Ctrl+Z and Ctrl+Shift+Z of the agent's last turn keep it under review take at most
  about 16 MB: 200 changes of 100 KB each kept 60 MB of them.
- Accept all, Keep Turn and Keep on a Replace in Project group of the Changes panel take
  thousands of changes in a large file at once instead of rewriting the rest of the file after
  each one. The review's own work for 5000 changes in a 60 000-line file (all of it for a file
  that is not open) takes about 10 ms instead of 0.15 s, for 20 000 about 50 ms instead of
  0.5 s.
- With thousands of changes under review in a large file, the review no longer works out again
  which words changed in each change after every typing pause, decision or agent edit: a change
  whose text is the same as before keeps its highlighted words. With 5000 changes in a
  60 000-line file, the update after a pause takes about 20 ms instead of 40 ms, with 20 000
  about 60 ms instead of 0.13 s. Once worked out in full, a change's highlighted words no longer
  depend on how busy the machine is.
- An agent edit in a large open file answers the agent sooner: the lines it quotes back are read
  alone, instead of copying the whole text and finding its lines again (5–11 ms in a 60 000-line
  file). Keeping a change, a file's or a turn's changes, or taking a keep back with Alt+U, copies
  the text of an open file once instead of twice (3.5–7 ms less in such a file).
- The check for undefined references and citations no longer goes through the whole project
  after every pause in typing, only through what changed: nothing after prose, and after an
  edit the file edited plus the references elsewhere to a label it added or removed. In a book
  of 300 chapters with 20 000 labels, 30 000 references and 40 000 citations, each pause cost
  the main thread 22–34 ms; now under 0.1 ms after prose and 0.5–3 ms after other edits. The
  inline equation numbers no longer go through the build's numbers for every open editor at
  each pause.
- With the Outline shown, an edit of one chapter computes only that chapter's rows again, and
  the outline's update leaves the other chapters' rows alone: in that book a new section took
  about 34 ms, now under 4 ms. A line break, a `\ref` or a `\label` typed in a paragraph (or a
  line break at the end of a heading's line), which moves rows or changes the chapter's entry
  in the project index without changing its rows, now costs under 0.3 ms instead of 34–40 ms.
- The inline equation numbers move with lines inserted or deleted above them just before the
  next frame, once for all the changes made until then (a paste over a selection, a replacement
  of several lines, Vim's `.`), instead of inside each change: with 5000 numbers, ten line
  changes at once cost 26 ms, now 3 ms, and a key that adds a line no longer spends 2.6 ms in
  its handler (the numbers move after it, before the frame).
- Opening a LaTeX document no longer loads GtkSourceView's LaTeX syntax definitions again each
  time (they were given to every new document and taken away at once, which freed them): a
  400-line chapter opens in 14 ms instead of 44 ms, and a session of 20 such tabs restores in
  0.3 s instead of 1.1 s.
- Vim's `:s` and `:%s` find all their matches at once and replace them in one edit, instead of
  searching the document again from its start after every replacement, with the window frozen
  meanwhile: `:%s` of 3300 matches in 10 000 lines takes 0.27 s instead of 18–50 s, a range of
  1000 of those lines 34 ms instead of 4–8 s, `:%s` of 20 000 matches in 60 000 lines, which
  did not finish in ten minutes, 1.5 s, and `:s` on a line near the end of that document 21 ms
  instead of about 0.4 s. Patterns, flags, ranges and where the cursor ends up are as before;
  `n` still finds the pattern and Ctrl+O still goes back, and it is still one undo step.
- After a large paste, the highlighting before the next frame colours the screen around the
  cursor instead of every pasted line: 0.75 ms instead of 35 ms for 1000 lines. The rest is
  coloured in the background.
- While no PDF is shown, switching to a tab no longer copies and indexes every open document to
  find its main document again when nothing changed since that tab was last shown, and copies
  the active document once instead of twice when something did.

### Fixed

- The agent's `get_review_state` no longer answers a large review with more than the agent
  can take: about 250 pending changes already passed Claude Code's limit for a tool's result
  (25,000 tokens), and 8000 changes in 301 files made a reply of 605 KB. It quotes at most 50
  changes of a file and about 48 KB of diff in all, and counts the rest with the lines they
  span, which `read_file` shows (that reply is now 52 KB, made in 5.5 ms instead of 40 ms); a
  change too long to quote by itself, such as a new file the agent wrote, is counted too.
- The word count of a project over 1 MiB whose main document is small (a book or thesis with
  `\include`d chapters) is no longer started when the main document is first shown: the size
  limit looked at the project index before it had read the chapters. texcount then kept a core
  busy at every start (31 s for 1.2 MB, 74 s for 11 MB). The item offers to count on a click,
  as for other large projects.
- A word count still running when Lemma quits is stopped: texcount ran on after the quit.
- Vim's `:s` no longer stops halfway, with a critical warning, or leaves a wrong text where a
  match that could have gone on (`ab?`, `(a)(b)?`) is directly followed by another one, and no
  longer hangs on some commands (`:2,4s/x|\s/<\1>/` on some texts never returned).
- An editor that went away with its hover popover still on it no longer hangs Lemma in an
  endless loop of "GtkPopover is not a child of LemmaTexView" warnings (seen with GTK's own
  input method, when no IBus or Fcitx module is installed).
- Hovering a formula, or the live preview, in a document whose text ends with `\def` (or
  `\gdef`, `\edef`, `\xdef`), as while one is typed at the end of a file, made Lemma quit.
- The math hover and the live preview of a file closed, changed on disk and opened again no
  longer use the preamble and definitions of the text it had before.
- With an input method that composes text (Japanese, Chinese or Korean input through IBus,
  Fcitx, macOS or Windows; dead keys), Enter in the agent's prompt box confirms the conversion
  instead of sending the prompt without the text being composed. Up, Down, Tab and Escape go to
  the input method too while it composes, and a permission card no longer takes the focus from
  the empty prompt box then (the next keys of the conversion, such as `n`, answered it).
- A table in an agent's answer no longer makes GTK warn ("Trying to measure GtkGrid …") each
  time it is laid out.
- Changes that other programs make to the files in the project's folder now reach completion,
  the undefined-reference check and the Outline without a build: a bibliography that a
  reference manager exports again, a chapter that is not open, edited in another editor or
  changed by `git checkout`, a file that an `\input` named before it existed, and images created
  or deleted (for `\includegraphics{`). A deleted chapter takes its labels with it.
- The same goes now for files outside the project's folder: a bibliography in a central folder
  (`\addbibresource{../central/refs.bib}`, or `refs/refs.bib` with `refs` a symbolic link to
  that folder, or `refs.bib` itself a link to a reference manager's export) or a chapter
  included through `../` or a link that another program changes, deletes or writes again
  reaches completion, the undefined-reference check and the Outline without a build (up to 32
  such folders are watched). Before, they were read again only after the next build.
- Figures and tables in chapters that are not open show their captions in the Outline (before,
  only with a `\label` after the caption, or the build's list of figures), a caption further
  than 40 lines below its `\begin{figure}` is found, and a `\caption*` gives a label in its float
  the title that completion and the label's hover show.
- Undefined references and citations are underlined and listed on the Problems page, and the
  inline equation numbers refreshed, about half a second after typing stops instead of about
  two seconds: the check no longer waits for the project's word list to be rebuilt.
- Theorem environments declared in a separate preamble file (`\newtheorem{mainthm}{Main
  Theorem}` in `preamble.tex`) appear in the Outline of the chapters that use them, open or
  not, and labels inside them get the theorem's title. Before, only the common names
  (`theorem`, `lemma`, …) did when the chapter was read before the preamble.
- Replace All and the agent's replace_all with thousands of matches no longer freeze the
  window. In a file under review (the agent's edits, Replace in Project, a reformat) the review
  takes all the changes of one replace at once, in linear time, and a file that is not open is
  rewritten in one pass instead of once per match: 20 000 matches in a 10 000-line file take
  0.45 s instead of 5.5 s with the file open, 0.13 s instead of 2.8 s with it closed. The line
  index no longer moves every line after each change, so the find bar's Replace All of 20 000
  matches in a 60 000-line file takes 0.6 s instead of 0.9 s. Alt+U of an Accept all of
  thousands of changes puts them back at once too (5000 changes: 21 ms instead of 0.27 s).
- Undoing and redoing a large agent edit keeps all of it under review. After Ctrl+Z and
  Ctrl+Shift+Z (Vim `u` and Ctrl+R) of a replace_all with more than 256 matches, only about
  256 of them came back as hunks: the review took the others for your own typing, so Reject
  all, Reject Turn and the Undo of Replace in Project could not take them back, and the undo
  told the agent that you had edited its changes. Every agent edit of the last turn is
  recognized now, up to 16 384 replacements in an open file (fewer when they are long: the
  records of the last turn take at most about 16 MB). Keeping them adds a few microseconds to a
  keystroke wherever it is typed.
- Redoing two agent edits that added the same text at one place, the second right after the
  first (`\beta` added to the end of a line twice, say), keeps both under review. After Ctrl+Z of
  both and Ctrl+Shift+Z (Vim `u` and Ctrl+R), the second was taken for your own typing: it was
  no longer a change to review, and Reject could not take it back.
- Reject all of thousands of changes, and its Ctrl+Z and Alt+U, no longer freeze the window:
  5000 changes take 0.05 s instead of 2.3 s, 0.07 s instead of 100 s and 0.18 s instead of
  1.1 s. Edits right after such a Reject all are no longer slowed down by it (a replace_all of
  20 000 matches: 0.07 s instead of 0.73 s), and the same goes for Reject Turn and the Undo of
  Replace in Project.
- Keeping or rejecting one change no longer lets Keep Turn and Reject Turn decide the change
  after it as the turn's alone when what the turn changed there is not known exactly (another
  edit cut into it): they leave it to be decided by hand, as before the first decision.
- An agent edit (or a Reject, a reformat) could change the wrong text when it came while Vim's
  insert mode was on and the `\end` of an environment whose `\begin` name was just typed had
  not followed yet: the rename went first, and the edit landed as many bytes off as the name
  grew or shrank. The edit now lands where it was meant. An agent edit that changes that very
  name is refused, and the agent is told that the text changed; a reformat is refused with a
  message. Reject (of a change, a file, a turn or everything), Alt+U and Review in the recovery
  bar let the rename go in first and then work on the text it leaves.
- An agent edit that only inserts or only deletes text could crash Lemma when a non-ASCII
  character (é, ∑, …) sat 256 bytes before it.
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
  When the next start cannot continue it after all, a note in the transcript says why, also
  when the project folder changed meanwhile (a file opened in another folder after every tab
  was closed) or another agent was chosen.
- An ACP agent other than Claude Code (Pi, Codex, Gemini CLI, …) that answered the first
  prompt with an error (a quota, an expired login) went without Lemma's instructions for the
  rest of the session; the next prompt now carries them.
- A file opened at a line behind the first one (`lemma main.tex chapter.tex:250`) that was not
  shown before the session was saved came back at that line next time, but scrolled to where
  the session before had left it, with the cursor out of view. It now comes back showing its
  line.
- In a project that includes more files than Lemma reads from disk (300), a reference to a
  label in one of the files it did not read was underlined and listed on the Problems page as
  undefined: a 1000-chapter book got thousands of them before its first build. References are
  not checked in such a project now (citations still are), and the log says so.

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
