//! chktex in the Problems page. The runner and parser are
//! `lemma_build::chktex`.
//!
//! - **When:** a LaTeX file (`.tex`, `.ltx`) open in an editor is linted [`DEBOUNCE`] after it
//!   was saved (`saver::connect_saved`: user saves, build saves, agent edits, rejects) and when
//!   an editor first shows it, so its warnings are there before the first save; again for every
//!   linted file when `[lint]` changes. Closing its last tab removes its findings: lint
//!   describes the open files (and costs nothing for the others). The text linted is the document's current text, piped into
//!   chktex on the jobs thread (`gio::spawn_blocking`), one run at a time per window; the
//!   working directory is the project root, so a project's `.chktexrc` applies.
//! - **Where:** the warnings become [`Finding`]s of `DiagSource::Lint` on the Problems page
//!   (their own section and filter, `diagnostics_panel`), mapped through the edits made while
//!   chktex ran, with a soft blue wavy underline under exactly the offending characters
//!   ([`STYLE`], below TeX errors' red and amber whole-line underlines) and a hover that names
//!   the warning ([`HOVER_PRIORITY`]).
//! - **Which:** `lemma_build::chktex::DEFAULT_SILENCED` (the noisiest warnings, chosen from the
//!   fixtures and real documents) are hidden unless `[lint] enabled` lists them; `[lint]
//!   disabled` hides more; `% chktex N` comments are chktex's own.
//! - **Disabling a warning** (row buttons and context menu): "Disable on This Line" adds `%
//!   chktex N` at the end of the line ([`disable_on_line`]; one undoable edit, saved when the
//!   document had no unsaved changes); "Disable Everywhere" adds N to `[lint] disabled`
//!   ([`disable_everywhere`], through `state.edit_config_file`, keeping comments).
//! - `[lint] chktex = false` turns it off (findings cleared); a missing `chktex` turns it off
//!   quietly for the window's lifetime.
//!
//! Hooks: [`install`] once per window; [`attach_editor`] per editor tab (called from
//! `diagnostics_panel::attach_editor`).

use crate::diagnostics_panel::{EditJournal, Finding, FindingAction, FindingStyle, Problem};
use crate::editor::{Editor, hover};
use crate::window::WindowState;
use crate::workspace::normalize;
use gtk::prelude::*;
use lemma_build::chktex::{self, LintWarning, RunError};
use lemma_config::{ConfigValue, LintCfg};
use lemma_core::{DiagSource, Origin};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Delay between a save (or an editor showing a file) and the lint.
pub const DEBOUNCE: Duration = Duration::from_millis(400);

/// The hover of a lint underline runs before the reference and math hovers: it only answers
/// on the few characters chktex points at.
pub const HOVER_PRIORITY: i32 = 300;

/// At most this many warnings of one file are listed.
pub const MAX_WARNINGS_PER_FILE: usize = 200;

/// The underline of lint findings: Adwaita's blue, wavy, a little transparent.
pub const STYLE: FindingStyle = FindingStyle {
    tag: "lemma-lint",
    color: (0.21, 0.52, 0.89, 0.85),
};

/// One window's linter.
struct Lint {
    state: Weak<WindowState>,
    program: RefCell<PathBuf>,
    /// `false` once chktex turned out to be missing.
    available: Cell<bool>,
    debounce: Cell<Duration>,
    timers: RefCell<HashMap<PathBuf, glib::SourceId>>,
    /// Files waiting for a run, in order.
    queue: RefCell<Vec<PathBuf>>,
    running: Cell<bool>,
    /// The `[lint]` settings the findings were made with.
    config: RefCell<LintCfg>,
    saved_hook: Cell<Option<crate::hooks::HookId>>,
    /// Runs finished since the window opened (tests wait for them).
    runs: Cell<u64>,
    /// Files whose tab closed while their lint ran: its result is dropped.
    closed: RefCell<Vec<PathBuf>>,
}

thread_local! {
    static LINTS: RefCell<Vec<Rc<Lint>>> = const { RefCell::new(Vec::new()) };
}

fn lint_of(state: &WindowState) -> Option<Rc<Lint>> {
    LINTS.with_borrow(|lints| {
        lints
            .iter()
            .find(|l| {
                l.state
                    .upgrade()
                    .is_some_and(|s| std::ptr::eq(Rc::as_ptr(&s), state))
            })
            .cloned()
    })
}

/// Called once per window after the M2 install hooks.
pub fn install(state: &Rc<WindowState>) {
    let lint = Rc::new(Lint {
        state: Rc::downgrade(state),
        program: RefCell::new(PathBuf::from("chktex")),
        available: Cell::new(true),
        debounce: Cell::new(DEBOUNCE),
        timers: RefCell::new(HashMap::new()),
        queue: RefCell::new(Vec::new()),
        running: Cell::new(false),
        config: RefCell::new(state.config.lint.clone()),
        saved_hook: Cell::new(None),
        runs: Cell::new(0),
        closed: RefCell::new(Vec::new()),
    });
    LINTS.with_borrow_mut(|lints| {
        lints.retain(|l| l.state.strong_count() > 0);
        lints.push(lint.clone());
    });
    if let Some(build) = state.build.get() {
        let panel = build.diagnostics();
        panel.set_finding_style(DiagSource::Lint, STYLE);
        panel.set_finding_actions(DiagSource::Lint, actions);
    }
    let weak = Rc::downgrade(&lint);
    lint.saved_hook
        .set(Some(crate::saver::connect_saved(move |path, _| {
            if let Some(lint) = weak.upgrade() {
                lint.schedule(path);
            }
        })));
    state.connect_config_changed(|state, file| {
        if file == crate::settings::ConfigFile::Config
            && let Some(lint) = lint_of(state)
        {
            lint.config_changed(state);
        }
    });
    let weak = Rc::downgrade(&lint);
    state.window.connect_destroy(move |_| {
        if let Some(lint) = weak.upgrade() {
            lint.close();
        }
    });
}

/// Called for every new editor tab (from `diagnostics_panel::attach_editor`): lints its file
/// and gives its view the hover of lint underlines.
pub fn attach_editor(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(lint) = lint_of(state) else {
        return;
    };
    lint.schedule(editor.document().path());
    // The hover registry is attached right after this hook.
    let (weak_state, weak_editor) = (Rc::downgrade(state), Rc::downgrade(editor));
    glib::idle_add_local_once(move || {
        let (Some(state), Some(editor)) = (weak_state.upgrade(), weak_editor.upgrade()) else {
            return;
        };
        let (weak_state, weak_editor) = (Rc::downgrade(&state), Rc::downgrade(&editor));
        hover::register(editor.view(), HOVER_PRIORITY, move |ctx| {
            let (state, editor) = (weak_state.upgrade()?, weak_editor.upgrade()?);
            let panel = state.build.get()?.diagnostics();
            let (finding, range) = panel.finding_at(&editor, ctx.offset)?;
            if finding.problem.source != DiagSource::Lint {
                return None;
            }
            ctx.set_range(range);
            Some(hover::HoverContent::Markup(hover_markup(&finding)))
        });
    });
}

/// Called when an editor tab closes (from `diagnostics_panel::detach_editor`): its file's lint
/// findings go with it (lint describes the files that are open), unless another tab shows it.
pub fn detach_editor(state: &Rc<WindowState>, editor: &Rc<Editor>) {
    let Some(lint) = lint_of(state) else {
        return;
    };
    let path = normalize(editor.document().path());
    let shown = state
        .editors()
        .iter()
        .any(|e| !Rc::ptr_eq(e, editor) && normalize(e.document().path()) == path);
    if shown {
        return;
    }
    if let Some(timer) = lint.timers.borrow_mut().remove(&path) {
        timer.remove();
    }
    lint.queue.borrow_mut().retain(|p| *p != path);
    lint.closed.borrow_mut().push(path.clone());
    if let Some(build) = state.build.get() {
        build
            .diagnostics()
            .set_findings(DiagSource::Lint, &path, Vec::new(), None);
    }
}

/// The hover text of a lint finding.
fn hover_markup(finding: &Finding) -> String {
    let message = glib::markup_escape_text(&finding.problem.message);
    match finding.code {
        Some(code) => {
            format!("{message}\n<span size=\"small\" alpha=\"65%\">chktex warning {code}</span>")
        }
        None => message.to_string(),
    }
}

/// Lints `path` now (tests; the app lints after saves and when editors open).
pub fn request(state: &Rc<WindowState>, path: &Path) {
    if let Some(lint) = lint_of(state) {
        lint.enqueue(normalize(path));
    }
}

/// True while nothing is scheduled, queued or running.
pub fn is_idle(state: &WindowState) -> bool {
    lint_of(state).is_none_or(|l| {
        l.timers.borrow().is_empty() && l.queue.borrow().is_empty() && !l.running.get()
    })
}

/// How many chktex runs finished in this window.
pub fn runs(state: &WindowState) -> u64 {
    lint_of(state).map_or(0, |l| l.runs.get())
}

/// `false` once chktex turned out to be missing (the lint is then off).
pub fn is_available(state: &WindowState) -> bool {
    lint_of(state).is_some_and(|l| l.available.get())
}

/// Uses `program` instead of `chktex` (tests: a missing program).
pub fn set_program(state: &WindowState, program: &Path) {
    if let Some(lint) = lint_of(state) {
        lint.program.replace(program.to_path_buf());
        lint.available.set(true);
    }
}

/// Changes the delay after saves (tests).
pub fn set_debounce(state: &WindowState, delay: Duration) {
    if let Some(lint) = lint_of(state) {
        lint.debounce.set(delay);
    }
}

/// The actions of a lint finding: silence it on its line (not for file-level warnings) or
/// everywhere.
fn actions(finding: &Finding) -> Vec<FindingAction> {
    let Some(code) = finding.code else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    if finding.columns.is_some() {
        actions.push(FindingAction {
            label: "Disable on This Line".to_owned(),
            tooltip: Some(format!("Adds “% chktex {code}” at the end of the line")),
            run: Rc::new(|state: &Rc<WindowState>, finding: &Finding| {
                disable_on_line(state, finding);
            }),
        });
    }
    actions.push(FindingAction {
        label: "Disable Everywhere".to_owned(),
        tooltip: Some(format!(
            "Never shows chktex warning {code} (adds it to [lint] disabled in config.toml)"
        )),
        run: Rc::new(move |state: &Rc<WindowState>, _: &Finding| {
            if let Err(e) = disable_everywhere(state, code) {
                state.toast(&e);
            }
        }),
    });
    actions
}

/// Adds `% chktex N` at the end of the finding's line (its line in the current text), as one
/// undoable edit; the document is saved when it had no unsaved changes (which lints it again),
/// else linted again at once.
pub fn disable_on_line(state: &Rc<WindowState>, finding: &Finding) {
    let (Some(code), Some(line)) = (finding.code, finding.problem.line) else {
        return;
    };
    let state = state.clone();
    let path = finding.problem.path.clone();
    glib::spawn_future_local(async move {
        let doc = match state.workspace.get_or_load(&path).await {
            Ok(doc) => doc,
            Err(e) => {
                state.toast(&e);
                return;
            }
        };
        let Some(text) = line.checked_sub(1).and_then(|l| doc.line_text(l)) else {
            return;
        };
        let start = doc.line_index().line_start(line - 1);
        let Some(edit) = chktex::suppress_on_line(&text, start, code) else {
            return;
        };
        let clean = !doc.is_dirty();
        if let Err(e) = doc.apply_edits(&[edit], Origin::User) {
            state.toast(&format!("Could not silence the warning: {e}"));
            return;
        }
        if clean {
            if let Err(e) = doc.save().await {
                state.toast(&format!("Could not save {}: {e}", doc.display_name()));
            }
        } else {
            request(&state, &path);
        }
    });
}

/// Adds warning `code` to `[lint] disabled` in the configuration file (comments and order
/// kept); the window applies it at once.
pub fn disable_everywhere(state: &Rc<WindowState>, code: u32) -> Result<(), String> {
    let mut disabled: Vec<i64> = state
        .config
        .lint
        .disabled
        .iter()
        .map(|&n| i64::from(n))
        .collect();
    if !disabled.contains(&i64::from(code)) {
        disabled.push(i64::from(code));
    }
    disabled.sort_unstable();
    state.edit_config_file(|text| {
        lemma_config::set_value(text, "lint", "disabled", ConfigValue::Ints(&disabled))
    })
}

impl Lint {
    fn enabled(&self, state: &WindowState) -> bool {
        self.available.get() && state.config.lint.chktex
    }

    /// Lints `path` [`DEBOUNCE`] from now (again, if it was scheduled).
    fn schedule(self: &Rc<Self>, path: &Path) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        if !self.enabled(&state) || !is_lintable(path) {
            return;
        }
        let path = normalize(path);
        // Only files open in an editor are linted (an agent's save of another file is not).
        if !state
            .editors()
            .iter()
            .any(|e| normalize(e.document().path()) == path)
        {
            return;
        }
        self.closed.borrow_mut().retain(|p| *p != path);
        if let Some(timer) = self.timers.borrow_mut().remove(&path) {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let key = path.clone();
        let timer = glib::timeout_add_local_once(self.debounce.get(), move || {
            if let Some(lint) = weak.upgrade() {
                lint.timers.borrow_mut().remove(&key);
                lint.enqueue(key);
            }
        });
        self.timers.borrow_mut().insert(path, timer);
    }

    fn enqueue(self: &Rc<Self>, path: PathBuf) {
        {
            let mut queue = self.queue.borrow_mut();
            if !queue.contains(&path) {
                queue.push(path);
            }
        }
        self.run_next();
    }

    /// Starts the next queued run unless one is running.
    fn run_next(self: &Rc<Self>) {
        if self.running.get() {
            return;
        }
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let path = {
            let mut queue = self.queue.borrow_mut();
            if queue.is_empty() {
                return;
            }
            queue.remove(0)
        };
        if !self.enabled(&state) {
            self.queue.borrow_mut().clear();
            return;
        }
        let Some(doc) = state.workspace.get(&path) else {
            self.run_next();
            return;
        };
        let text = doc.text();
        let journal = EditJournal::start(std::slice::from_ref(&doc));
        let root = state.workspace.root().path().to_path_buf();
        let (cwd, name) = match path.strip_prefix(&root) {
            Ok(rel) => (root.clone(), rel.to_string_lossy().into_owned()),
            Err(_) => (
                path.parent()
                    .map_or_else(|| root.clone(), Path::to_path_buf),
                doc.display_name(),
            ),
        };
        let program = self.program.borrow().clone();
        self.running.set(true);
        let lint = self.clone();
        glib::spawn_future_local(async move {
            let job = {
                let text = text.clone();
                gio::spawn_blocking(move || chktex::run(&program, &text, &cwd, &name))
            };
            let result = job
                .await
                .unwrap_or_else(|_| Err(RunError::Failed("chktex panicked".into())));
            lint.running.set(false);
            lint.runs.set(lint.runs.get() + 1);
            lint.finished(&path, &text, result, journal);
            lint.run_next();
        });
    }

    fn finished(
        &self,
        path: &Path,
        text: &str,
        result: Result<Vec<LintWarning>, RunError>,
        journal: EditJournal,
    ) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Some(panel) = state.build.get().map(|b| b.diagnostics()) else {
            return;
        };
        match result {
            Ok(warnings) => {
                if !self.enabled(&state) || self.closed.borrow().iter().any(|p| p == path) {
                    return;
                }
                let cfg = state.config.lint.clone();
                let findings = findings(path, text, &warnings, &cfg);
                panel.set_findings(DiagSource::Lint, path, findings, Some(journal));
            }
            Err(RunError::NotFound) => {
                tracing::info!("chktex is not installed: lint is off");
                self.available.set(false);
                self.queue.borrow_mut().clear();
                panel.clear_findings(DiagSource::Lint);
            }
            Err(e) => tracing::warn!("lint of {}: {e}", path.display()),
        }
    }

    /// `[lint]` changed: off clears everything; otherwise every linted file and every open
    /// LaTeX file is linted again with the new settings.
    fn config_changed(self: &Rc<Self>, state: &Rc<WindowState>) {
        let now = state.config.lint.clone();
        if *self.config.borrow() == now {
            return;
        }
        self.config.replace(now);
        let Some(panel) = state.build.get().map(|b| b.diagnostics()) else {
            return;
        };
        if !self.enabled(state) {
            for (_, timer) in self.timers.borrow_mut().drain() {
                timer.remove();
            }
            self.queue.borrow_mut().clear();
            panel.clear_findings(DiagSource::Lint);
            return;
        }
        let mut paths = panel.finding_files(DiagSource::Lint);
        for editor in state.editors() {
            let path = normalize(editor.document().path());
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        for path in paths.into_iter().filter(|p| is_lintable(p)) {
            self.enqueue(path);
        }
    }

    fn close(&self) {
        if let Some(hook) = self.saved_hook.take() {
            crate::saver::disconnect_saved(hook);
        }
        for (_, timer) in self.timers.borrow_mut().drain() {
            timer.remove();
        }
        self.queue.borrow_mut().clear();
        LINTS.with_borrow_mut(|lints| lints.retain(|l| !std::ptr::eq(Rc::as_ptr(l), self)));
    }
}

/// LaTeX sources chktex understands (not `.sty`/`.cls`, which are full of `@` internals, nor
/// `.bib`).
fn is_lintable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "tex" | "ltx"))
}

/// The shown warnings of `text` (the file `path`) as findings: `[lint]` applied, columns
/// clamped to the lines, at most [`MAX_WARNINGS_PER_FILE`].
pub fn findings(path: &Path, text: &str, warnings: &[LintWarning], cfg: &LintCfg) -> Vec<Finding> {
    let lines: Vec<&str> = text.split('\n').collect();
    warnings
        .iter()
        .filter(|w| chktex::is_shown(w.number, &cfg.disabled, &cfg.enabled))
        .take(MAX_WARNINGS_PER_FILE)
        .map(|w| {
            let line_text = lines.get(w.line as usize - 1).copied().unwrap_or("");
            let columns = w.byte_range(line_text).map(|r| {
                // An empty range (at the end of the line) still marks one character before it.
                let start = if r.is_empty() && r.start > 0 {
                    line_text[..r.start]
                        .char_indices()
                        .last()
                        .map_or(r.start, |(i, _)| i)
                } else {
                    r.start
                };
                start as u32..r.end as u32
            });
            let d = chktex::to_diagnostic(w, path);
            Finding {
                problem: Problem {
                    path: d.path,
                    line: d.line,
                    severity: d.severity,
                    message: d.message,
                    context: d.context,
                    source: d.source,
                },
                columns,
                code: Some(w.number),
            }
        })
        .collect()
}
