//! The word count.
//!
//! - **The document:** `texcount -merge -sum -sub=section ROOT` (run in the root's directory, on
//!   the jobs thread) after each successful build of the root (`BuildController::
//!   connect_finished`), and once when a root document is first shown. Its "Sum count" is what
//!   `texcount -1 -sum -merge ROOT` prints: words in text, headings and captions plus one per
//!   formula. The status bar's slot `state.status.word_count` then shows "1 234 words" (a narrow
//!   no-break space groups the digits); it stays hidden before the first count, while
//!   `[status] word_count` is false, and for good when `texcount` is missing.
//! - **Cost:** texcount is slow (about 80 kB of source per second), so these automatic counts
//!   skip projects larger than [`AUTO_MAX_BYTES`] (the item then reads "Count Words" and counts
//!   on a click). The size is that of the root and the files it includes as they are on disk,
//!   found on the jobs thread ([`project_size`]): when a root is first shown, the project index
//!   has not read its includes yet. A newer count stops a running one, and closing the window
//!   or quitting stops it too: texcount is killed at once ([`Stop`]), so it never outlives
//!   Lemma.
//! - **The breakdown:** clicking the item opens a popover with the parts of the total and one
//!   row per part, chapter and section (texcount's subcounts, `\section{…}\label{…}` titles
//!   cleaned), plus the selection's count.
//! - **The selection:** `win.word-count` ([`show`]) opens the same popover; with text selected
//!   it counts the selection (`texcount -1 -sum -utf8 -` on the selected text) on demand.
//!   Without the status item (no count yet, or `[status] word_count = false`) the result is a
//!   toast.
//!
//! Hooks: [`install`] once per window; [`show`] is the action.

use crate::window::WindowState;
use adw::prelude::*;
use lemma_build::BuildStatus;
use lemma_syntax::ProjectIndex;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Styles of the status item and its popover.
const CSS: &str = "
.lemma-wordcount { padding: 0 6px; min-height: 0; }
.lemma-wordcount-popover { min-width: 280px; }
.lemma-wordcount-popover .lemma-wordcount-total { font-size: 1.25em; font-weight: 700; }
.lemma-wordcount-popover row { padding: 2px 4px; min-height: 0; }
";

/// One part of the document (texcount's subcounts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionCount {
    /// "Section", "Chapter", "Part", … or empty for the text before the first heading.
    pub kind: String,
    /// The heading, cleaned (`Introduction}\label{sec:intro` → `Introduction`).
    pub title: String,
    /// Words in text, headings and captions plus one per formula, as in the sum.
    pub words: u64,
}

/// What texcount counted in a document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WordCount {
    /// texcount's "Sum count" (`texcount -1 -sum`).
    pub total: u64,
    pub text: u64,
    pub headers: u64,
    pub captions: u64,
    pub inline_formulas: u64,
    pub displayed_formulas: u64,
    pub sections: Vec<SectionCount>,
    /// texcount reported problems (a file it could not include).
    pub errors: bool,
}

/// Why texcount could not count.
#[derive(Debug)]
pub enum CountError {
    NotFound,
    /// A newer count, or the window closing, stopped it.
    Cancelled,
    /// An automatic count of a project larger than [`AUTO_MAX_BYTES`].
    TooLarge,
    Failed(String),
}

impl std::fmt::Display for CountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CountError::NotFound => f.write_str("texcount is not installed"),
            CountError::Cancelled => f.write_str("the count was stopped"),
            CountError::TooLarge => f.write_str("the project is large; click to count"),
            CountError::Failed(e) => write!(f, "texcount failed: {e}"),
        }
    }
}

/// "1 234": the digits grouped by three with a narrow no-break space.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202F}');
        }
        out.push(c);
    }
    out
}

/// "1 word", "1 234 words".
pub fn words_label(n: u64) -> String {
    format!(
        "{} {}",
        format_count(n),
        if n == 1 { "word" } else { "words" }
    )
}

/// Parses texcount's output for one file with `-sum -sub=…` (or without subcounts).
pub fn parse(output: &str) -> Option<WordCount> {
    let mut count = WordCount::default();
    let mut total = None;
    let mut in_subcounts = false;
    for line in output.lines() {
        let number =
            |prefix: &str| -> Option<u64> { line.strip_prefix(prefix)?.trim().parse().ok() };
        if let Some(n) = number("Sum count:") {
            total = Some(n);
        } else if let Some(n) = number("Words in text:") {
            count.text = n;
        } else if let Some(n) = number("Words in headers:") {
            count.headers = n;
        } else if let Some(n) = number("Words outside text (captions, etc.):") {
            count.captions = n;
        } else if let Some(n) = number("Number of math inlines:") {
            count.inline_formulas = n;
        } else if let Some(n) = number("Number of math displayed:") {
            count.displayed_formulas = n;
        } else if line.starts_with("Subcounts:") {
            in_subcounts = true;
        } else if line.contains("(errors:") || line.starts_with("!!!") {
            count.errors = true;
        } else if in_subcounts && let Some(section) = parse_subcount(line) {
            count.sections.push(section);
        }
    }
    count.total = total?;
    Some(count)
}

/// `  39+1+0 (1/0/2/0) Section: Introduction}\label{sec:intro`.
fn parse_subcount(line: &str) -> Option<SectionCount> {
    let line = line.trim();
    let (sums, rest) = line.split_once(' ')?;
    let words: Vec<u64> = sums
        .split('+')
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    let rest = rest.trim_start();
    let (counts, title) = rest.strip_prefix('(')?.split_once(')')?;
    let counts: Vec<u64> = counts
        .split('/')
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    // text + headers + captions, plus inline and displayed formulas (as in the sum).
    let formulas = counts.get(2).copied().unwrap_or(0) + counts.get(3).copied().unwrap_or(0);
    let words = words.iter().sum::<u64>() + formulas;
    let title = title.trim();
    let (kind, title) = match title.split_once(": ") {
        _ if title == "_top_" => (String::new(), String::new()),
        Some((kind, title)) => (kind.to_owned(), clean_title(title)),
        None => (String::new(), clean_title(title)),
    };
    Some(SectionCount { kind, title, words })
}

/// A heading as texcount prints it, up to the brace that closes it.
fn clean_title(title: &str) -> String {
    let mut depth = 0i32;
    for (i, c) in title.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return title[..i].trim().to_owned(),
            '}' => depth -= 1,
            _ => {}
        }
    }
    title.trim().to_owned()
}

/// Automatic counts (after builds, when a root is shown) are skipped for projects whose sources
/// are larger than this: texcount reads about 80 kB per second (27 s for the 2.2 MB
/// `large.tex`). The status item then offers to count on a click.
pub const AUTO_MAX_BYTES: u64 = 1 << 20;

/// Stops a document count from another thread: [`Stop::stop`] kills texcount right away, so a
/// count stopped by the window closing or the application quitting (whose jobs thread ends with
/// the process) leaves no texcount running.
#[derive(Debug, Default)]
pub struct Stop {
    stopped: AtomicBool,
    /// texcount, while it runs.
    child: Mutex<Option<Child>>,
}

impl Stop {
    /// Stops the count ([`CountError::Cancelled`]).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(child) = self.lock().as_mut() {
            let _ = child.kill();
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Child>> {
        self.child.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Counts `root` and the files it includes (blocking; in the root's directory). [`Stop::stop`]
/// on `stop` kills texcount ([`CountError::Cancelled`]).
pub fn count_document(program: &Path, root: &Path, stop: &Stop) -> Result<WordCount, CountError> {
    let dir = root.parent().unwrap_or(Path::new("."));
    let name = root
        .file_name()
        .ok_or_else(|| CountError::Failed("no file name".into()))?;
    let mut file = std::ffi::OsString::from("./");
    file.push(name);
    if stop.is_stopped() {
        return Err(CountError::Cancelled);
    }
    let mut child = lemma_core::process::command(program)
        .args(["-merge", "-sum", "-sub=section"])
        .arg(&file)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(spawn_error)?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    stop.lock().replace(child);
    let status = loop {
        let mut guard = stop.lock();
        if stop.is_stopped() {
            // Out of the lock first: `Stop::stop` on the main thread must not wait for this.
            let mut child = guard.take().expect("the running texcount");
            drop(guard);
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(CountError::Cancelled);
        }
        let waited = guard.as_mut().expect("the running texcount").try_wait();
        match waited {
            Ok(Some(status)) => {
                guard.take();
                break status;
            }
            Ok(None) => {
                drop(guard);
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                guard.take();
                return Err(CountError::Failed(e.to_string()));
            }
        }
    };
    let out = reader.join().unwrap_or_default();
    let out = String::from_utf8_lossy(&out);
    parse(&out).ok_or_else(|| CountError::Failed(format!("unexpected output ({status})")))
}

/// The size in bytes of `root` and the files it includes (`\input`, `\include`, `\subfile`,
/// `\import`, resolved as the project index resolves them), as they are on disk; once the sum
/// passes `limit`, the sum so far. Blocking: reads and indexes the files, but none whose size
/// would pass `limit`, so at most `limit` bytes. Missing files, and anything but a regular file
/// (a FIFO, a device), count 0 and are not read.
pub fn project_size(root: &Path, limit: u64) -> u64 {
    let mut index = ProjectIndex::default();
    let mut sizes: HashMap<PathBuf, u64> = HashMap::new();
    loop {
        let mut total = 0;
        let mut unread = Vec::new();
        for file in index.project_files(root) {
            total += *sizes.entry(file.clone()).or_insert_with(|| {
                std::fs::metadata(&file)
                    .ok()
                    .filter(std::fs::Metadata::is_file)
                    .map_or(0, |m| m.len())
            });
            if total > limit {
                return total;
            }
            if !index.files.contains_key(&file) {
                unread.push(file);
            }
        }
        if unread.is_empty() {
            return total;
        }
        for file in unread {
            let text = read_source(&file, limit);
            index.update_file(file, Arc::new(lemma_syntax::index_file(&text)));
        }
    }
}

/// The text of the regular file at `path`, at most `cap` bytes, with invalid UTF-8 replaced (a
/// Latin-1 chapter includes its files as a UTF-8 one does); empty for anything else. Opening a
/// FIFO does not wait for a writer, and a device such as `/dev/zero` is not read.
fn read_source(path: &Path, cap: u64) -> String {
    let Ok(file) = crate::fsutil::open_nonblocking(path) else {
        return String::new();
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.take(cap).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Counts the words of `text` (a selection; blocking).
pub fn count_text(program: &Path, text: &str) -> Result<u64, CountError> {
    let mut child = lemma_core::process::command(program)
        .args(["-1", "-sum", "-utf8", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(spawn_error)?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = text.as_bytes().to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut out = String::new();
    let read = child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut out);
    let _ = writer.join();
    let _ = child.wait();
    read.map_err(|e| CountError::Failed(e.to_string()))?;
    parse_single(&out).ok_or_else(|| CountError::Failed(format!("unexpected output {out:?}")))
}

/// The number `texcount -1` prints (maybe followed by `(errors:1)`).
pub fn parse_single(output: &str) -> Option<u64> {
    output
        .split_whitespace()
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

fn spawn_error(e: io::Error) -> CountError {
    match e.kind() {
        io::ErrorKind::NotFound => CountError::NotFound,
        _ => CountError::Failed(e.to_string()),
    }
}

// ----- the status item -----

struct Counter {
    state: Weak<WindowState>,
    program: RefCell<PathBuf>,
    available: Cell<bool>,
    button: gtk::MenuButton,
    label: gtk::Label,
    popover: gtk::Popover,
    content: gtk::Box,
    last: RefCell<Option<(PathBuf, WordCount)>>,
    /// A count runs (stop it with its [`Stop`]); `again` asks for another one after it (of that
    /// root; `true` for an automatic one).
    running: RefCell<Option<Arc<Stop>>>,
    again: RefCell<Option<(PathBuf, bool)>>,
    /// The counts [`show`] runs for a toast, while they run.
    asked: RefCell<Vec<Arc<Stop>>>,
    /// The window closed or the application quit: no more counts.
    ended: Cell<bool>,
    /// A root too large to count automatically: the item offers a count on a click.
    offer: RefCell<Option<PathBuf>>,
    /// The selection's count (and the text it counted), once computed.
    selection: RefCell<Option<(String, Result<u64, String>)>>,
    selection_running: Cell<bool>,
    runs: Cell<u64>,
}

thread_local! {
    static COUNTERS: RefCell<Vec<Rc<Counter>>> = const { RefCell::new(Vec::new()) };
}

fn counter_of(state: &WindowState) -> Option<Rc<Counter>> {
    COUNTERS.with_borrow(|counters| {
        counters
            .iter()
            .find(|c| {
                c.state
                    .upgrade()
                    .is_some_and(|s| std::ptr::eq(Rc::as_ptr(&s), state))
            })
            .cloned()
    })
}

/// Called once per window after the M2 install hooks; fills `state.status.word_count`.
pub fn install(state: &Rc<WindowState>) {
    crate::build_ui::install_css_once("wordcount", CSS);
    let label = gtk::Label::builder().css_classes(["numeric"]).build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    let popover = gtk::Popover::builder()
        .child(&content)
        .css_classes(["lemma-wordcount-popover"])
        .build();
    let button = gtk::MenuButton::builder()
        .child(&label)
        .popover(&popover)
        .direction(gtk::ArrowType::Up)
        .tooltip_text("Word Count")
        .css_classes(["flat", "lemma-wordcount"])
        .build();
    state.status.word_count.set_child(Some(&button));
    let counter = Rc::new(Counter {
        state: Rc::downgrade(state),
        program: RefCell::new(PathBuf::from("texcount")),
        available: Cell::new(true),
        button,
        label,
        popover: popover.clone(),
        content,
        last: RefCell::new(None),
        running: RefCell::new(None),
        again: RefCell::new(None),
        asked: RefCell::new(Vec::new()),
        ended: Cell::new(false),
        offer: RefCell::new(None),
        selection: RefCell::new(None),
        selection_running: Cell::new(false),
        runs: Cell::new(0),
    });
    COUNTERS.with_borrow_mut(|counters| {
        counters.retain(|c| c.state.strong_count() > 0);
        counters.push(counter.clone());
    });
    let weak = Rc::downgrade(&counter);
    popover.connect_show(move |_| {
        if let Some(counter) = weak.upgrade() {
            let offered = counter.offer.borrow().clone();
            if let Some(root) = offered {
                counter.count(root, false);
            }
            counter.count_selection();
            counter.fill_popover();
        }
    });
    // Quitting ends the process without destroying the windows (the jobs thread goes with it,
    // texcount would not): a count still running is stopped then, or with its window.
    let weak = Rc::downgrade(&counter);
    let quit = state.app.connect_shutdown(move |_| {
        if let Some(counter) = weak.upgrade() {
            counter.end();
        }
    });
    let (weak, app) = (Rc::downgrade(&counter), state.app.clone());
    let quit = Cell::new(Some(quit));
    state.window.connect_destroy(move |_| {
        if let Some(id) = quit.take() {
            app.disconnect(id);
        }
        if let Some(counter) = weak.upgrade() {
            counter.end();
            COUNTERS.with_borrow_mut(|c| c.retain(|x| !Rc::ptr_eq(x, &counter)));
        }
    });
    if let Some(build) = state.build.get() {
        let weak = Rc::downgrade(state);
        build.connect_finished(move |outcome| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if outcome.status == BuildStatus::Success
                && let (Some(counter), Some(root)) =
                    (counter_of(&state), state.build.get().and_then(|b| b.root()))
            {
                counter.count(root, true);
            }
        });
    }
    // The first root document shown is counted before any build.
    let weak = Rc::downgrade(state);
    state.tab_view.connect_selected_page_notify(move |_| {
        if let Some(state) = weak.upgrade()
            && let Some(counter) = counter_of(&state)
        {
            counter.count_shown_root(&state);
        }
    });
    state.connect_config_changed(|state, file| {
        if file == crate::settings::ConfigFile::Config
            && let Some(counter) = counter_of(state)
        {
            counter.refresh_visibility(state);
        }
    });
    counter.refresh_visibility(state);
}

/// `win.word-count`: the word count of the selection (with text selected) and of the document,
/// in the status item's popover (or a toast when the item is hidden).
pub fn show(state: &Rc<WindowState>) {
    let Some(counter) = counter_of(state) else {
        return;
    };
    if !counter.available.get() {
        state.toast("Counting words needs texcount, which is not installed");
        return;
    }
    if counter.button.is_visible() && counter.button.is_mapped() {
        counter.popover.popup();
        return;
    }
    // No status item: count and tell.
    let weak = Rc::downgrade(state);
    let selection = selected_text(state);
    let program = counter.program.borrow().clone();
    let root = counter
        .last
        .borrow()
        .as_ref()
        .map(|(p, _)| p.clone())
        .or_else(|| state.build.get().and_then(|b| b.root()))
        .or_else(|| {
            state
                .active_editor()
                .map(|e| e.document().path().to_path_buf())
        });
    glib::spawn_future_local(async move {
        let message = match selection {
            Some(text) => gio::spawn_blocking(move || count_text(&program, &text))
                .await
                .map_err(|_| CountError::Failed("panicked".into()))
                .and_then(|r| r)
                .map(|n| format!("Selection: {}", words_label(n))),
            None => match root {
                Some(root) => {
                    // Stopped with the window too (`Counter::end`).
                    let stop = Arc::new(Stop::default());
                    counter.asked.borrow_mut().push(stop.clone());
                    let counting = stop.clone();
                    let result =
                        gio::spawn_blocking(move || count_document(&program, &root, &counting))
                            .await;
                    counter
                        .asked
                        .borrow_mut()
                        .retain(|s| !Arc::ptr_eq(s, &stop));
                    result
                        .map_err(|_| CountError::Failed("panicked".into()))
                        .and_then(|r| r)
                        .map(|c| format!("Document: {}", words_label(c.total)))
                }
                None => Err(CountError::Failed("there is no document".into())),
            },
        };
        if let Some(state) = weak.upgrade() {
            match message {
                Ok(m) => state.toast(&m),
                Err(e) => state.toast(&format!("Could not count words: {e}")),
            }
        }
    });
}

/// Counts the root document `root` now (tests; the app counts after successful builds).
pub fn count_now(state: &WindowState, root: &Path) {
    if let Some(counter) = counter_of(state) {
        counter.count(root.to_path_buf(), false);
    }
}

/// The last count of the document, with the root it counted.
pub fn last_count(state: &WindowState) -> Option<(PathBuf, WordCount)> {
    counter_of(state).and_then(|c| c.last.borrow().clone())
}

/// The status item's text ("1 234 words"), while it is shown.
pub fn status_text(state: &WindowState) -> Option<String> {
    let counter = counter_of(state)?;
    state
        .status
        .word_count
        .is_visible()
        .then(|| counter.label.text().to_string())
}

/// The count of the selection the popover shows (after [`show`] with text selected).
pub fn selection_count(state: &WindowState) -> Option<Result<u64, String>> {
    counter_of(state).and_then(|c| c.selection.borrow().as_ref().map(|(_, r)| r.clone()))
}

/// The popover with the breakdown (tests).
pub fn popover(state: &WindowState) -> Option<gtk::Popover> {
    counter_of(state).map(|c| c.popover.clone())
}

/// True while no count runs.
pub fn is_idle(state: &WindowState) -> bool {
    counter_of(state).is_none_or(|c| c.running.borrow().is_none() && !c.selection_running.get())
}

/// How many document counts finished in this window.
pub fn runs(state: &WindowState) -> u64 {
    counter_of(state).map_or(0, |c| c.runs.get())
}

/// Uses `program` instead of `texcount` (tests: a missing program).
pub fn set_program(state: &WindowState, program: &Path) {
    if let Some(counter) = counter_of(state) {
        counter.program.replace(program.to_path_buf());
        counter.available.set(true);
    }
}

/// The selected text of the active editor, if any.
fn selected_text(state: &WindowState) -> Option<String> {
    let editor = state.active_editor()?;
    let buffer = editor.buffer();
    let (start, end) = buffer.selection_bounds()?;
    let text = buffer.text(&start, &end, false).to_string();
    (!text.trim().is_empty()).then_some(text)
}

/// Whether `doc` is a root document: `\documentclass` and `\begin{document}` within its
/// first lines (read line by line, without copying the text).
fn is_root_document(doc: &crate::document::Document) -> bool {
    let lines = doc.line_index().line_count().min(ROOT_SCAN_LINES);
    let mut class = false;
    for i in 0..lines {
        let Some(line) = doc.line_text(i) else {
            break;
        };
        let code = line.split('%').next().unwrap_or("");
        class |= code.contains("\\documentclass");
        if class && code.contains("\\begin{document}") {
            return true;
        }
    }
    false
}

/// How many lines [`is_root_document`] reads.
const ROOT_SCAN_LINES: u32 = 2000;

impl Counter {
    /// The window closed or the application quits: stops the running counts (texcount is
    /// killed) and starts none after them.
    fn end(&self) {
        self.ended.set(true);
        self.again.take();
        if let Some(stop) = self.running.borrow().as_ref() {
            stop.stop();
        }
        for stop in self.asked.borrow().iter() {
            stop.stop();
        }
    }

    fn refresh_visibility(&self, state: &WindowState) {
        let shown = self.available.get()
            && state.config.status.word_count
            && (self.last.borrow().is_some() || self.offer.borrow().is_some());
        state.status.word_count.set_visible(shown);
        if !shown {
            self.popover.popdown();
        }
    }

    /// Counts the shown document when it is a root and nothing was counted yet.
    fn count_shown_root(self: &Rc<Self>, state: &WindowState) {
        if self.last.borrow().is_some()
            || self.offer.borrow().is_some()
            || self.running.borrow().is_some()
            || !self.available.get()
        {
            return;
        }
        let Some(editor) = state.active_editor() else {
            return;
        };
        let doc = editor.document();
        if is_root_document(doc) && doc.path().is_absolute() {
            self.count(doc.path().to_path_buf(), true);
        }
    }

    /// Counts `root` on the jobs thread. An `automatic` count (after a build, a root shown)
    /// of a project larger than [`AUTO_MAX_BYTES`] only offers a count on a click. A count
    /// requested while one runs stops that one and runs after it.
    fn count(self: &Rc<Self>, root: PathBuf, automatic: bool) {
        if !self.available.get() || self.ended.get() {
            return;
        }
        if let Some(stop) = self.running.borrow().as_ref() {
            stop.stop();
            self.again.replace(Some((root, automatic)));
            return;
        }
        let stop = Arc::new(Stop::default());
        self.running.replace(Some(stop.clone()));
        let program = self.program.borrow().clone();
        let counter = self.clone();
        glib::spawn_future_local(async move {
            let target = root.clone();
            let result = gio::spawn_blocking(move || {
                // The sources texcount will read, for the size limit.
                if automatic && project_size(&target, AUTO_MAX_BYTES) > AUTO_MAX_BYTES {
                    return Err(CountError::TooLarge);
                }
                count_document(&program, &target, &stop)
            })
            .await
            .unwrap_or_else(|_| Err(CountError::Failed("texcount panicked".into())));
            counter.running.replace(None);
            counter.runs.set(counter.runs.get() + 1);
            match result {
                Ok(count) => {
                    counter.label.set_text(&words_label(count.total));
                    counter.button.set_tooltip_text(Some(&format!(
                        "{} — Show Breakdown",
                        words_label(count.total)
                    )));
                    counter.offer.replace(None);
                    counter.last.replace(Some((root, count)));
                    if counter.popover.is_visible() {
                        counter.fill_popover();
                    }
                }
                Err(CountError::TooLarge) => {
                    if counter
                        .last
                        .borrow()
                        .as_ref()
                        .is_none_or(|(r, _)| *r != root)
                    {
                        counter.label.set_text("Count Words");
                        counter
                            .button
                            .set_tooltip_text(Some("A large project: click to count its words"));
                        counter.last.replace(None);
                    }
                    counter.offer.replace(Some(root));
                }
                Err(CountError::NotFound) => {
                    tracing::info!("texcount is not installed: no word count");
                    counter.available.set(false);
                }
                Err(CountError::Cancelled) => {}
                Err(e) => tracing::warn!("word count of {}: {e}", root.display()),
            }
            if let Some(state) = counter.state.upgrade() {
                counter.refresh_visibility(&state);
            }
            if let Some((next, automatic)) = counter.again.take() {
                counter.count(next, automatic);
            }
        });
    }

    /// Counts the active editor's selection (when it changed since the last count) and shows
    /// it in the popover.
    fn count_selection(self: &Rc<Self>) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(text) = selected_text(&state) else {
            self.selection.replace(None);
            return;
        };
        if self
            .selection
            .borrow()
            .as_ref()
            .is_some_and(|(counted, _)| *counted == text)
            || self.selection_running.get()
        {
            return;
        }
        self.selection_running.set(true);
        let program = self.program.borrow().clone();
        let counter = self.clone();
        glib::spawn_future_local(async move {
            let input = text.clone();
            let result = gio::spawn_blocking(move || count_text(&program, &input))
                .await
                .unwrap_or_else(|_| Err(CountError::Failed("texcount panicked".into())))
                .map_err(|e| e.to_string());
            counter.selection_running.set(false);
            counter.selection.replace(Some((text, result)));
            if counter.popover.is_visible() {
                counter.fill_popover();
            }
        });
    }

    /// The popover's content: the total, its parts, the sections and the selection.
    fn fill_popover(&self) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
        let last = self.last.borrow();
        let Some((root, count)) = last.as_ref() else {
            let text = if self.running.borrow().is_some() {
                "Counting words…"
            } else {
                "Not counted yet"
            };
            self.content.append(
                &gtk::Label::builder()
                    .label(text)
                    .css_classes(["dim-label"])
                    .build(),
            );
            return;
        };
        let header = gtk::Box::new(gtk::Orientation::Vertical, 2);
        header.append(
            &gtk::Label::builder()
                .label(words_label(count.total))
                .xalign(0.0)
                .css_classes(["lemma-wordcount-total", "numeric"])
                .build(),
        );
        let name = root
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let mut parts = vec![
            format!("{} in text", format_count(count.text)),
            format!("{} in headings", format_count(count.headers)),
        ];
        if count.captions > 0 {
            parts.push(format!("{} in captions", format_count(count.captions)));
        }
        let formulas = count.inline_formulas + count.displayed_formulas;
        if formulas > 0 {
            parts.push(format!(
                "{} {}",
                format_count(formulas),
                if formulas == 1 { "formula" } else { "formulas" }
            ));
        }
        header.append(
            &gtk::Label::builder()
                .label(format!("{name} · {}", parts.join(" · ")))
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(40)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        if count.errors {
            header.append(
                &gtk::Label::builder()
                    .label("texcount could not read every included file")
                    .xalign(0.0)
                    .css_classes(["caption", "warning"])
                    .build(),
            );
        }
        self.content.append(&header);
        let sections: Vec<&SectionCount> = count
            .sections
            .iter()
            .filter(|s| s.words > 0 || !s.title.is_empty())
            .collect();
        if !sections.is_empty() {
            let list = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(["boxed-list"])
                .build();
            for section in sections {
                let title = if section.kind.is_empty() {
                    "Before the first heading".to_owned()
                } else {
                    section.title.clone()
                };
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                row.set_margin_top(4);
                row.set_margin_bottom(4);
                row.set_margin_start(8);
                row.set_margin_end(8);
                let name = gtk::Label::builder()
                    .label(&title)
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(pango::EllipsizeMode::End)
                    .max_width_chars(36)
                    .tooltip_text(if section.kind.is_empty() {
                        title.clone()
                    } else {
                        format!("{}: {}", section.kind, section.title)
                    })
                    .build();
                if section.kind == "Part" || section.kind == "Chapter" {
                    name.add_css_class("heading");
                }
                if section.kind.is_empty() {
                    name.add_css_class("dim-label");
                }
                row.append(&name);
                row.append(
                    &gtk::Label::builder()
                        .label(format_count(section.words))
                        .css_classes(["numeric", "dim-label"])
                        .build(),
                );
                list.append(&row);
            }
            let scroller = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(360)
                .child(&list)
                .build();
            self.content.append(&scroller);
        }
        let selection = match self.selection.borrow().as_ref() {
            Some((_, Ok(n))) => Some(format!("Selection: {}", words_label(*n))),
            Some((_, Err(e))) => Some(format!("Selection: {e}")),
            None if self.selection_running.get() => Some("Counting the selection…".to_owned()),
            None => None,
        };
        if let Some(selection) = selection {
            self.content.append(
                &gtk::Label::builder()
                    .label(selection)
                    .xalign(0.0)
                    .css_classes(["numeric"])
                    .build(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped() {
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(1234), "1\u{202F}234");
        assert_eq!(format_count(1_234_567), "1\u{202F}234\u{202F}567");
        assert_eq!(words_label(1), "1 word");
        assert_eq!(words_label(1112), "1\u{202F}112 words");
    }

    #[test]
    fn parses_texcount() {
        let out = "File: main.tex\nEncoding: utf8\nSum count: 1112\nWords in text: 1014\n\
                   Words in headers: 34\nWords outside text (captions, etc.): 0\n\
                   Number of headers: 13\nNumber of floats/tables/figures: 0\n\
                   Number of math inlines: 48\nNumber of math displayed: 16\nSubcounts:\n\
                   \x20 text+headers+captions (#headers/#floats/#inlines/#displayed)\n\
                   \x20 24+6+0 (2/0/0/0) _top_\n\
                   \x20 39+1+0 (1/0/2/0) Section: Introduction}\\label{sec:intro\n\
                   \x20 46+3+0 (1/0/14/2) Section: The {Yoneda} lemma}\\label{sec:yoneda\n";
        let c = parse(out).unwrap();
        assert_eq!(
            (
                c.total,
                c.text,
                c.headers,
                c.inline_formulas,
                c.displayed_formulas
            ),
            (1112, 1014, 34, 48, 16)
        );
        assert_eq!(
            c.sections,
            vec![
                SectionCount {
                    kind: String::new(),
                    title: String::new(),
                    words: 30
                },
                SectionCount {
                    kind: "Section".into(),
                    title: "Introduction".into(),
                    words: 42
                },
                SectionCount {
                    kind: "Section".into(),
                    title: "The {Yoneda} lemma".into(),
                    words: 65
                },
            ]
        );
        assert!(!c.errors);
        assert_eq!(parse("nothing"), None);
        assert_eq!(parse_single("1047\n (errors:1)\n"), Some(1047));
        assert_eq!(parse_single("7\n"), Some(7));
    }

    /// The size counts the root's includes, read from disk (nested, `\include` and `\input`,
    /// with or without `.tex`), once each, and stops reading past the limit.
    #[test]
    fn project_size_follows_the_includes() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            text.len() as u64
        };
        let root = write(
            "main.tex",
            "\\documentclass{book}\n\\begin{document}\n\\include{ch/one}\n\\input{ch/two.tex}\n\
             \\input{ch/one}\n\\input{missing}\n\\end{document}\n",
        );
        let one = write("ch/one.tex", "One.\n\\input{ch/deep}\n");
        let two = write("ch/two.tex", &"Two words. ".repeat(100));
        let deep = write("ch/deep.tex", "Deep.\n");
        write("unrelated.tex", &"x".repeat(5000));
        let main = dir.path().join("main.tex");
        let all = root + one + two + deep;
        assert_eq!(project_size(&main, u64::MAX), all);
        assert_eq!(project_size(&main, all), all);
        // Past the limit: what was summed when it was passed.
        assert_eq!(project_size(&main, root + one), root + one + two);
        assert_eq!(project_size(&main, 10), root);
        assert_eq!(project_size(&dir.path().join("none.tex"), 10), 0);

        // Not regular files: never read (a FIFO would block, /dev/zero never ends).
        #[cfg(unix)]
        {
            let fifo = dir.path().join("fifo.tex");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap();
            assert!(made.success());
            let zero = dir.path().join("zero.tex");
            std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
            assert_eq!(read_source(&fifo, 1024), "");
            assert_eq!(read_source(&zero, 1024), "");
            let odd = dir.path().join("odd.tex");
            let size = write("odd.tex", "\\input{fifo}\n\\input{zero}\n");
            assert_eq!(project_size(&odd, 1 << 20), size);
        }
    }
}
