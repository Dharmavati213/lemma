# Changelog

Releases are named after Greek letters; the version number is in `Cargo.toml`. Each release's
notes, with its downloads, are in [`docs/releases/`](docs/releases/) and on the
[releases page](https://github.com/dharmavati213/lemma/releases).

## Unreleased

### Added

- Two buttons at the start of the PDF toolbar jump between the source and the PDF. Show Page in
  Source opens the source line at the middle of the page on screen (the page number the toolbar
  shows), and Show Cursor in PDF scrolls the PDF to the cursor's line and flashes it, as
  Ctrl+Alt+J does. Show PDF Page in Source is in the command palette too, and the buttons work
  in the PDF's own window. Ctrl+click in the PDF still opens the line of the point clicked.

### Changed

- Enter takes the first word proposed in prose too, as it already did for commands, references
  and citations: Enter after `Lor` gives `Lorem`. After a word that is already whole (one the
  spell checker accepts or the project uses, such as `the` with `theorem` proposed), Enter still
  breaks the line, and so does every Enter in prose while spell checking is off. Up and Down
  pick another proposal; Escape closes the popup.

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
