//! Build wiring (ARCHITECTURE §6): the build queue on the main thread, the build button and state
//! indicator, the status-bar build label, and handing results to the PDF pane and the Problems
//! list.
//!
//! A request ([`BuildController::request`]) first saves every dirty document (latexmk decides by
//! content), then finds the root document and engine on the jobs thread (`lemma_build`'s rules
//! over a `lemma_syntax` index of the open documents and nearby files), then queues the build:
//! one runs, one more may wait, and a newer request replaces the waiting one. Each build runs on
//! its own thread; its outcome comes back over a channel. After a build that wrote a PDF (success,
//! or TeX errors with output) the preview reloads it as a new generation, and the diagnostics
//! replace the previous ones. A build that left the PDF as it was (latexmk found nothing to do)
//! reloads nothing: the build thread stamps the PDF ([`lemma_build::PdfStamp`]), and the preview
//! compares the stamp with the one of the PDF it opened last.
//!
//! A failed build is toasted; each build's result replaces the toast of the one before, so a
//! series of failing builds (repeated saves, auto-save, the agent's `compile` calls) never queues
//! up stale toasts, and a build that succeeds or is stopped withdraws the old one. A latexmk that
//! is not installed is explained once for the automatic builds (F5 always says it); after that
//! the status bar says so. Clean Build Files and Rebuild (`win.clean-build`) stops the running
//! build and runs `latexmk -c` on the next build's thread before latexmk builds.

use crate::diagnostics_panel::{self, DiagnosticsPanel, EditJournal, Problem};
use crate::saver::SaveError;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_build::{
    Build, BuildOutcome, BuildQueue, BuildRequest, BuildStatus, Canceller, Engine, PdfStamp,
    RootGuess, RootSource, detect_engine, detect_root,
};
use lemma_syntax::{FileIndex, IncludeKind, ProjectIndex, index_file};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Read};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Why a build was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// F5 or the build button.
    Manual,
    /// A user save (Ctrl+S, `:w`) with `build.on_save`.
    Save,
    /// The agent's `compile` tool.
    AgentCompile,
    /// An agent turn that changed files ended (`build.on_agent_turn_end`).
    AgentTurnEnd,
}

/// Default accelerators of the actions this module adds (`[keys]` in config.toml wins).
/// `win.clean-build` has none: it is rare, and the command palette has it.
const ACCELS: &[(&str, &[&str])] = &[("win.stop-build", &["<Shift>F5"])];

/// What a build says when latexmk does not start because it is not installed (or not on PATH).
/// MiKTeX's latexmk also needs a Perl that MiKTeX does not bring; TeX Live's has its own.
pub const LATEXMK_MISSING: &str = "latexmk was not found: install TeX Live, MacTeX or MiKTeX \
                                   (with Perl on Windows), or set [build] latexmk in config.toml";

const CSS: &str = ".lemma-build-indicator { padding: 2px 6px; }";

/// Directory levels above a document that are searched for its root document.
const ROOT_SEARCH_LEVELS: usize = 2;
/// At most this many files are read to find the root and the `\include` directories.
const MAX_INDEXED_FILES: usize = 300;
/// Larger files are not indexed.
const MAX_INDEXED_BYTES: u64 = 1 << 20;

/// What a build thread reports.
enum BuildMsg {
    Started(Canceller),
    Finished(Box<Finished>),
}

struct Finished {
    outcome: BuildOutcome,
    problems: Vec<Problem>,
}

/// The build being run.
struct Running {
    request: BuildRequest,
    reason: Reason,
    /// `latexmk -c` runs first (Clean Build Files and Rebuild).
    clean: bool,
    canceller: Option<Canceller>,
    stop_requested: bool,
    /// The edits made since the build's sources were saved.
    journal: Option<EditJournal>,
}

type FinishedFn = dyn Fn(&BuildOutcome);

/// Starts and tracks builds for one window (stored in `WindowState::build`).
pub struct BuildController {
    inner: Rc<Inner>,
}

struct Inner {
    state: Weak<WindowState>,
    queue: RefCell<BuildQueue>,
    /// True while a request is saving and detecting its root.
    preparing: Cell<bool>,
    /// A request that arrived while another one was preparing.
    again: Cell<Option<Reason>>,
    /// Incremented by every Stop: a request that was being prepared meanwhile is dropped.
    stop_generation: Cell<u64>,
    /// Why the waiting build (in `queue`) was requested.
    waiting_reason: Cell<Reason>,
    /// The edits made since the waiting build's sources were saved.
    waiting_journal: RefCell<Option<EditJournal>>,
    /// The waiting build starts with `latexmk -c`.
    waiting_clean: Cell<bool>,
    running: RefCell<Option<Running>>,
    /// The root of the last build, or the user's answer to "which is the main document?".
    remembered_root: RefCell<Option<PathBuf>>,
    /// The user's answers to "which is the main document?", by the directory of the file they
    /// were given for. They apply to the files in that directory and below it.
    chosen_roots: RefCell<HashMap<PathBuf, PathBuf>>,
    last: RefCell<Option<BuildOutcome>>,
    finished: RefCell<Vec<Rc<FinishedFn>>>,
    /// The toast of the last build or failed request, while it is shown or waits to be.
    toast: RefCell<Option<adw::Toast>>,
    /// The next request that is queued starts with `latexmk -c` (Clean Build Files and
    /// Rebuild).
    clean_next: Cell<bool>,
    /// The missing latexmk was toasted; automatic builds now only say so in the status bar.
    /// Cleared by a build whose latexmk started.
    latexmk_missing_told: Cell<bool>,
    diagnostics: DiagnosticsPanel,
    indicator: Indicator,
}

/// The header-bar build state: a spinner while building, then ✓ or ✗ with counts.
struct Indicator {
    button: gtk::Button,
    spinner: adw::Spinner,
    icon: gtk::Image,
    errors: gtk::Label,
    warning_icon: gtk::Image,
    warnings: gtk::Label,
}

impl BuildController {
    /// Asks for a build of the active document's root. Dirty documents are saved first. One
    /// build runs at a time; a request made meanwhile waits, replacing any waiting one.
    pub fn request(&self, reason: Reason) {
        self.inner.request(reason);
    }

    /// Stops the running build (killing its whole process group) and drops the waiting one and
    /// any request still saving or finding its root.
    pub fn stop(&self) {
        self.inner.stop();
    }

    /// The toast of the last build (or of a request that could not start) while it is shown or
    /// waits to be shown; `None` once it is dismissed or withdrawn.
    pub fn toast(&self) -> Option<adw::Toast> {
        self.inner.toast.borrow().clone()
    }

    /// True while a build runs.
    pub fn is_running(&self) -> bool {
        self.inner.running.borrow().is_some()
    }

    /// True while a request is being prepared or a build runs or waits.
    pub fn is_busy(&self) -> bool {
        self.inner.preparing.get() || self.inner.queue.borrow().is_running()
    }

    /// The outcome of the last finished build.
    pub fn last_outcome(&self) -> Option<BuildOutcome> {
        self.inner.last.borrow().clone()
    }

    /// Calls `f` after every finished build, after the diagnostics and the PDF were updated.
    pub fn connect_finished(&self, f: impl Fn(&BuildOutcome) + 'static) {
        self.inner.finished.borrow_mut().push(Rc::new(f));
    }

    /// The Problems page, marks and underlines.
    pub fn diagnostics(&self) -> &DiagnosticsPanel {
        &self.inner.diagnostics
    }

    /// The root document of the last build (or the one the user chose).
    pub fn root(&self) -> Option<PathBuf> {
        self.inner.remembered_root.borrow().clone()
    }

    /// Finds the active document's root and shows the PDF its last build left (in the output
    /// directory, else next to the root), unless the preview already shows or loads that PDF.
    /// Resolves to the PDF's path.
    pub fn show_existing_pdf(&self) -> impl Future<Output = Result<PathBuf, String>> + use<> {
        let inner = self.inner.clone();
        async move { inner.show_existing_pdf().await }
    }
}

impl Inner {
    /// [`BuildController::request`].
    fn request(self: &Rc<Self>, reason: Reason) {
        let inner = self.clone();
        if inner.preparing.replace(true) {
            inner.again.set(Some(reason));
            return;
        }
        // Taken now: a Stop that comes before the future first runs must count too.
        let mut generation = inner.stop_generation.get();
        glib::spawn_future_local(async move {
            let mut reason = reason;
            loop {
                let prepared = inner.prepare(reason).await;
                let stopped = inner.stop_generation.get() != generation;
                match prepared {
                    Ok(_) if stopped => {
                        tracing::info!("build request dropped: Stop was pressed while saving");
                    }
                    Ok((request, journal)) => inner.enqueue(request, reason, journal),
                    Err(message) => {
                        // A clean-up asked for with this request is dropped with it.
                        if !stopped {
                            inner.clean_next.set(false);
                        }
                        if let Some(message) = message {
                            inner.fail(&message);
                        }
                    }
                }
                // Stop clears `again`, so a request found here came after any Stop so far.
                match inner.again.take() {
                    Some(next) => {
                        reason = next;
                        generation = inner.stop_generation.get();
                    }
                    None => break,
                }
            }
            inner.preparing.set(false);
        });
    }

    /// Clean Build Files and Rebuild: stops the running build (and drops any other), then builds
    /// as F5 does, with `latexmk -c` first. The clean-up runs on that build's thread once the
    /// stopped build has ended, for the root, engine and output directory that build uses, so
    /// no other latexmk ever runs beside it. It goes with that build: when the build cannot
    /// start, or a Stop comes before it starts, there is no clean-up either; a Stop while it
    /// runs stops it, and there is no build.
    fn clean_and_rebuild(self: &Rc<Self>) {
        self.stop();
        self.clean_next.set(true);
        self.request(Reason::Manual);
    }

    /// Saves the dirty documents and works out what to build. `Err(None)` means there is
    /// nothing more to report. The journal records the edits made from the save on, so the
    /// build's line numbers can be mapped to the text when it finishes.
    ///
    /// Without a detectable root, only a manual build (F5) asks which file is the main
    /// document; other builds just say so in the status bar.
    async fn prepare(
        self: &Rc<Self>,
        reason: Reason,
    ) -> Result<(BuildRequest, EditJournal), Option<String>> {
        let state = self.state.upgrade().ok_or(None)?;
        if let Err(failures) = state.workspace.save_all_dirty().await {
            let names: Vec<String> = failures
                .iter()
                .map(|(path, e)| format!("{}: {e}", display_name(path)))
                .collect();
            let message = format!("Not building: could not save {}", names.join(", "));
            // A file another program changed gets the Overwrite offer of a user save, which
            // builds again once it is overwritten; other failures are reported as they are.
            let mut others = false;
            for (path, e) in &failures {
                match state.workspace.get(path) {
                    Some(doc) if *e == SaveError::ExternallyModified => {
                        state.offer_overwrite_and_build(&doc, reason);
                    }
                    _ => others = true,
                }
            }
            if others {
                return Err(Some(message));
            }
            state.status.build.set_text(&message);
            return Err(None);
        }
        let journal = EditJournal::start(&state.workspace.documents());
        let mut detection = self.detect_for_active(&state, None).await?;
        if detection.root.is_none() {
            if reason != Reason::Manual {
                state
                    .status
                    .build
                    .set_text("Not built: no main document (F5 to choose one)");
                return Err(None);
            }
            let chosen = self
                .ask_root(&state)
                .await
                .ok_or_else(|| Some("Could not find the main document to build".to_owned()))?;
            if let Some(dir) = state
                .active_editor()
                .and_then(|e| e.document().path().parent().map(Path::to_path_buf))
            {
                self.chosen_roots.borrow_mut().insert(dir, chosen.clone());
            }
            detection = self.detect_for_active(&state, Some(chosen)).await?;
        }
        let root = detection.root.ok_or(None)?;
        if !detection.root_exists {
            return Err(Some(format!(
                "The main document {} does not exist",
                root.display()
            )));
        }
        self.remembered_root.replace(Some(root.clone()));
        let cfg = &state.config.build;
        let request = BuildRequest {
            root,
            engine: detection.engine,
            outdir: cfg.outdir.clone(),
            mirror_dirs: detection.mirror_dirs,
            timeout: Duration::from_secs(cfg.timeout_s),
            latexmk: cfg.latexmk.clone(),
        };
        Ok((request, journal))
    }

    /// Runs root and engine detection for the active document (or the remembered root when no
    /// document is open) on the jobs thread.
    async fn detect_for_active(
        &self,
        state: &Rc<WindowState>,
        forced_root: Option<PathBuf>,
    ) -> Result<Detection, Option<String>> {
        let remembered = self.remembered_root.borrow().clone();
        let (file, text) = match state.active_editor() {
            Some(editor) => {
                let doc = editor.document();
                (doc.path().to_path_buf(), doc.text())
            }
            None => match forced_root.clone().or_else(|| remembered.clone()) {
                Some(root) => (root, String::new()),
                None => return Err(Some("Open a document to build".to_owned())),
            },
        };
        let open = state
            .workspace
            .documents()
            .iter()
            .map(|d| (d.path().to_path_buf(), d.text()))
            .collect();
        let chosen = {
            let chosen_roots = self.chosen_roots.borrow();
            file.ancestors()
                .skip(1)
                .find_map(|dir| chosen_roots.get(dir))
                .cloned()
        };
        let input = DetectInput {
            file,
            text,
            open,
            remembered,
            chosen,
            forced_root,
        };
        gio::spawn_blocking(move || detect(input))
            .await
            .map_err(|_| Some("Root detection failed unexpectedly".to_owned()))
    }

    /// Asks the user once which file is the main document (interactive runs only).
    async fn ask_root(&self, state: &Rc<WindowState>) -> Option<PathBuf> {
        if state.scripted {
            return None;
        }
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("LaTeX Documents"));
        filter.add_pattern("*.tex");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let folder = state
            .active_editor()
            .and_then(|e| e.document().path().parent().map(Path::to_path_buf))
            .unwrap_or_else(|| state.workspace.root().path().to_path_buf());
        let dialog = gtk::FileDialog::builder()
            .title("Choose the Main Document")
            .modal(true)
            .filters(&filters)
            .default_filter(&filter)
            .initial_folder(&gio::File::for_path(folder))
            .build();
        let file = dialog.open_future(Some(&state.window)).await.ok()?;
        file.path()
    }

    fn enqueue(self: &Rc<Self>, request: BuildRequest, reason: Reason, journal: EditJournal) {
        let clean = self.clean_next.take();
        let start = self.queue.borrow_mut().request(request);
        match start {
            Some(request) => self.start(request, reason, Some(journal), clean),
            None => {
                self.waiting_reason.set(reason);
                self.waiting_journal.replace(Some(journal));
                // A newer request replaces the waiting one, but not its clean-up.
                self.waiting_clean.set(self.waiting_clean.get() || clean);
                self.show_running();
            }
        }
    }

    /// Runs `request` on a new thread (after `latexmk -c` when `clean` is set) and handles its
    /// messages on the main thread.
    fn start(
        self: &Rc<Self>,
        request: BuildRequest,
        reason: Reason,
        journal: Option<EditJournal>,
        clean: bool,
    ) {
        tracing::info!(
            "building {} ({:?}, {reason:?}{})",
            request.root.display(),
            request.engine,
            if clean { ", cleaned first" } else { "" }
        );
        self.running.replace(Some(Running {
            request: request.clone(),
            reason,
            clean,
            canceller: None,
            stop_requested: false,
            journal,
        }));
        self.show_running();
        let (tx, rx) = async_channel::unbounded();
        let thread_request = request.clone();
        let spawned = std::thread::Builder::new()
            .name("build".into())
            .spawn(move || run_build(&thread_request, clean, &tx));
        if let Err(e) = spawned {
            let outcome = BuildOutcome::spawn_failed(&e);
            self.finish(Finished {
                outcome,
                problems: Vec::new(),
            });
            return;
        }
        let inner = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(message) = rx.recv().await {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                match message {
                    BuildMsg::Started(canceller) => inner.started(canceller),
                    BuildMsg::Finished(finished) => {
                        inner.finish(*finished);
                        return;
                    }
                }
            }
        });
    }

    /// A latexmk of the running build started (the build's, or `latexmk -c` before it).
    fn started(&self, canceller: Canceller) {
        // A latexmk runs, so a missing one is worth explaining again. Only a start says so: a
        // build that ends without one (no build thread, an output directory that cannot be
        // created, a latexmk that cannot be executed) says nothing about whether it is
        // installed.
        self.latexmk_missing_told.set(false);
        let mut running = self.running.borrow_mut();
        if let Some(running) = running.as_mut() {
            if running.stop_requested {
                canceller.cancel();
            }
            running.canceller = Some(canceller);
        }
    }

    fn stop(&self) {
        self.stop_generation
            .set(self.stop_generation.get().wrapping_add(1));
        self.again.set(None);
        self.clean_next.set(false);
        self.queue.borrow_mut().clear_waiting();
        self.waiting_journal.take();
        self.waiting_clean.set(false);
        let running = {
            let mut running = self.running.borrow_mut();
            if let Some(running) = running.as_mut() {
                running.stop_requested = true;
                if let Some(canceller) = &running.canceller {
                    canceller.cancel();
                }
            }
            running.is_some()
        };
        // Only a request being prepared was stopped: say so, as for a running build.
        if !running
            && self.preparing.get()
            && let Some(state) = self.state.upgrade()
        {
            state.status.build.set_text("Build stopped");
        }
        self.show_running();
    }

    fn finish(self: &Rc<Self>, finished: Finished) {
        let Finished { outcome, problems } = finished;
        let running = self.running.take();
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let (root, reason, journal) = match running {
            Some(running) => (running.request.root, running.reason, running.journal),
            None => (PathBuf::new(), Reason::Manual, None),
        };
        tracing::info!(
            "build of {} finished: {:?} in {:.1?}",
            root.display(),
            outcome.status,
            outcome.elapsed
        );
        // A stopped or timed-out build's log is incomplete: keep the previous diagnostics.
        if matches!(outcome.status, BuildStatus::Success | BuildStatus::Errors) {
            match journal {
                Some(journal) => self.diagnostics.set_from_build(problems, &root, journal),
                None => self.diagnostics.set(problems, &root),
            }
        }
        let has_output = outcome.status == BuildStatus::Success
            || (outcome.status == BuildStatus::Errors && outcome.exit_code == Some(12));
        if has_output && let (Some(pdf), Some(controller)) = (&outcome.pdf, state.pdf.get()) {
            controller.open_if_changed(pdf, outcome.pdf_stamp);
        }
        self.last.replace(Some(outcome.clone()));
        let next = self.queue.borrow_mut().finished();
        self.show_result(&state, &outcome, &root, reason);
        let observers = self.finished.borrow().clone();
        for observer in observers {
            observer(&outcome);
        }
        if let Some(next) = next {
            let journal = self.waiting_journal.take();
            let clean = self.waiting_clean.take();
            self.start(next, self.waiting_reason.get(), journal, clean);
        }
    }

    /// Reports a request that could not start.
    fn fail(self: &Rc<Self>, message: &str) {
        if let Some(state) = self.state.upgrade() {
            let toast = adw::Toast::builder()
                .title(message)
                .use_markup(false)
                .build();
            self.replace_toast(&state, Some(toast));
            state.status.build.set_text(message);
        }
    }

    /// Shows `toast` in place of the toast of the build before, which is dismissed whether it
    /// is on screen or still waits behind other toasts; `None` only withdraws that one.
    fn replace_toast(self: &Rc<Self>, state: &WindowState, toast: Option<adw::Toast>) {
        let old = self.toast.replace(toast.clone());
        // Dismissed outside the borrow: `dismissed` handlers run right away.
        if let Some(old) = old {
            old.dismiss();
        }
        let Some(toast) = toast else {
            return;
        };
        let weak = Rc::downgrade(self);
        toast.connect_dismissed(move |toast| {
            if let Some(inner) = weak.upgrade() {
                let mut current = inner.toast.borrow_mut();
                if current.as_ref() == Some(toast) {
                    current.take();
                }
            }
        });
        state.toasts.add_toast(toast);
    }

    async fn show_existing_pdf(self: &Rc<Self>) -> Result<PathBuf, String> {
        let state = self.state.upgrade().ok_or("the window is gone")?;
        let detection = self
            .detect_for_active(&state, None)
            .await
            .map_err(|e| e.unwrap_or_else(|| "the window is gone".into()))?;
        let root = detection.root.ok_or("could not find the main document")?;
        if self.remembered_root.borrow().is_none() {
            self.remembered_root.replace(Some(root.clone()));
        }
        let outdir = state.config.build.outdir.clone();
        let (pdf, modified, stamp) = gio::spawn_blocking(move || {
            let pdf = existing_pdf(&root, &outdir)?;
            let modified = std::fs::metadata(&pdf).and_then(|m| m.modified()).ok();
            let stamp = PdfStamp::of(&pdf);
            Some((pdf, modified, stamp))
        })
        .await
        .ok()
        .flatten()
        .ok_or("no PDF from an earlier build")?;
        let controller = state.pdf.get().ok_or("no PDF preview")?;
        // With its stamp, so that a first build that finds nothing to do does not reload it.
        if controller.requested_pdf().as_deref() != Some(pdf.as_path()) {
            controller.open_if_changed(&pdf, stamp);
        }
        // Until this session builds, say where the PDF on screen comes from.
        let idle = self.last.borrow().is_none()
            && self.running.borrow().is_none()
            && !self.preparing.get();
        if idle {
            state.status.build.set_text(&earlier_pdf_text(modified));
        }
        Ok(pdf)
    }

    fn show_running(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let running = self.running.borrow();
        let Some(running) = running.as_ref() else {
            return;
        };
        let name = display_name(&running.request.root);
        let building = if running.clean {
            format!("Cleaning and building {name}…")
        } else {
            format!("Building {name}…")
        };
        let text = if running.stop_requested {
            "Stopping build…".to_owned()
        } else if self.queue.borrow().waiting().is_some() {
            format!("{building} (another build waits)")
        } else {
            building
        };
        state.status.build.set_text(&text);
        let i = &self.indicator;
        i.button.set_visible(true);
        i.button.set_tooltip_text(Some(&text));
        i.spinner.set_visible(true);
        for widget in [
            i.icon.upcast_ref::<gtk::Widget>(),
            i.errors.upcast_ref(),
            i.warning_icon.upcast_ref(),
            i.warnings.upcast_ref(),
        ] {
            widget.set_visible(false);
        }
        let button = &state.header.build_button;
        button.set_icon_name("media-playback-stop-symbolic");
        button.set_tooltip_text(Some("Stop Build (Shift+F5)"));
        button.set_action_name(Some("win.stop-build"));
    }

    fn show_result(
        self: &Rc<Self>,
        state: &Rc<WindowState>,
        outcome: &BuildOutcome,
        root: &Path,
        reason: Reason,
    ) {
        if self.running.borrow().is_none() {
            let button = &state.header.build_button;
            button.set_icon_name("media-playback-start-symbolic");
            button.set_tooltip_text(Some("Build (F5)"));
            button.set_action_name(Some("win.build"));
        }
        let (errors, warnings, bad_boxes) = self.diagnostics.counts();
        let text = status_text(outcome, errors, warnings);
        state.status.build.set_text(&text);
        let detail = diagnostics_panel::summary(errors, warnings, bad_boxes);
        let missing = latexmk_missing(outcome);
        let tooltip = if missing {
            LATEXMK_MISSING.to_owned()
        } else if detail.is_empty() || !text.starts_with("Built") {
            text.clone()
        } else {
            format!("{text}\n{detail}")
        };
        let i = &self.indicator;
        i.button.set_visible(true);
        i.button.set_tooltip_text(Some(&tooltip));
        i.spinner.set_visible(false);
        for class in ["success", "error", "dim-label"] {
            i.icon.remove_css_class(class);
        }
        let (icon, class) = match outcome.status {
            BuildStatus::Success => ("object-select-symbolic", "success"),
            BuildStatus::Cancelled => ("media-playback-stop-symbolic", "dim-label"),
            BuildStatus::Errors | BuildStatus::TimedOut | BuildStatus::SpawnFailed(_) => {
                ("dialog-error-symbolic", "error")
            }
        };
        i.icon.set_icon_name(Some(icon));
        i.icon.add_css_class(class);
        i.icon.set_visible(true);
        let failed = outcome.status == BuildStatus::Errors;
        i.errors.set_text(&errors.to_string());
        i.errors.set_visible(failed && errors > 0);
        i.warnings.set_text(&warnings.to_string());
        let show_warnings =
            warnings > 0 && matches!(outcome.status, BuildStatus::Success | BuildStatus::Errors);
        i.warning_icon.set_visible(show_warnings);
        i.warnings.set_visible(show_warnings);

        // Without latexmk every save would toast the same advice: F5 shows it each time, the
        // automatic builds once (until a latexmk starts again, see `started`). Otherwise the
        // status bar says it, and a toast still showing it stays; another build's (a latexmk
        // that could not be executed) is replaced by none, as by any build.
        if missing {
            let told = self.latexmk_missing_told.replace(true);
            if told && reason != Reason::Manual {
                let stale = self
                    .toast
                    .borrow()
                    .as_ref()
                    .is_some_and(|t| t.title().as_deref() != Some(LATEXMK_MISSING));
                if stale {
                    self.replace_toast(state, None);
                }
                return;
            }
        }
        let toast = match &outcome.status {
            BuildStatus::Errors => Some((
                match errors {
                    0 => format!("Build of {} failed", display_name(root)),
                    1 => "Build failed: 1 error".to_owned(),
                    n => format!("Build failed: {n} errors"),
                },
                true,
            )),
            BuildStatus::TimedOut => Some((
                format!(
                    "Build timed out after {} s and was stopped",
                    state.config.build.timeout_s
                ),
                false,
            )),
            BuildStatus::SpawnFailed(error) if missing => Some((error.clone(), false)),
            BuildStatus::SpawnFailed(error) => Some((format!("Could not build: {error}"), false)),
            BuildStatus::Success | BuildStatus::Cancelled => None,
        };
        let toast = toast.map(|(title, show_problems)| {
            let toast = adw::Toast::builder().title(title).use_markup(false).build();
            if show_problems {
                toast.set_button_label(Some("_Show"));
                let weak = Rc::downgrade(state);
                toast.connect_button_clicked(move |_| {
                    if let Some(state) = weak.upgrade() {
                        diagnostics_panel::reveal(&state);
                    }
                });
            }
            toast
        });
        self.replace_toast(state, toast);
    }
}

/// Whether `outcome` is a build whose latexmk is not installed ([`LATEXMK_MISSING`]).
fn latexmk_missing(outcome: &BuildOutcome) -> bool {
    matches!(&outcome.status, BuildStatus::SpawnFailed(error) if error == LATEXMK_MISSING)
}

/// The status-bar text after a build.
pub fn status_text(outcome: &BuildOutcome, errors: usize, warnings: usize) -> String {
    let seconds = outcome.elapsed.as_secs_f64();
    match &outcome.status {
        BuildStatus::Success if warnings > 0 => {
            let noun = if warnings == 1 { "warning" } else { "warnings" };
            format!("Built in {seconds:.1} s · {warnings} {noun}")
        }
        BuildStatus::Success => format!("Built in {seconds:.1} s"),
        BuildStatus::Errors => {
            let summary = diagnostics_panel::summary(errors, warnings, 0);
            if summary.is_empty() {
                "Build failed".to_owned()
            } else {
                format!("Build failed: {summary}")
            }
        }
        BuildStatus::Cancelled => "Build stopped".to_owned(),
        BuildStatus::TimedOut => "Build timed out".to_owned(),
        BuildStatus::SpawnFailed(_) if latexmk_missing(outcome) => {
            "Not built: latexmk not found".to_owned()
        }
        BuildStatus::SpawnFailed(_) => "Build could not start".to_owned(),
    }
}

/// The status-bar text while the preview shows a PDF that an earlier session built, last
/// modified at `modified`: "PDF from 14:32" (today), "PDF from 9 Oct 14:32" (this year), else
/// "PDF from 9 Oct 2025".
fn earlier_pdf_text(modified: Option<SystemTime>) -> String {
    let when = modified
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .and_then(|secs| glib::DateTime::from_unix_local(secs).ok());
    let Some(when) = when else {
        return "PDF from an earlier build".to_owned();
    };
    let now = glib::DateTime::now_local().ok();
    let format = match now {
        Some(now) if now.year() == when.year() && now.day_of_year() == when.day_of_year() => {
            "%H:%M"
        }
        Some(now) if now.year() == when.year() => "%-d %b %H:%M",
        _ => "%-d %b %Y",
    };
    match when.format(format) {
        Ok(text) => format!("PDF from {text}"),
        Err(_) => "PDF from an earlier build".to_owned(),
    }
}

/// The build thread: runs `latexmk -c` first when `clean` is set, starts latexmk, reports its
/// canceller (the clean-up's first), waits, and maps the diagnostics to problems (which touches
/// the file system, so it happens here rather than on the main thread). A failed clean-up is the
/// build's outcome: the files it was to remove would make the build fail again. A stopped one
/// ends the build.
fn run_build(request: &BuildRequest, clean: bool, tx: &async_channel::Sender<BuildMsg>) {
    // The receiver only goes away with the window; the build is still waited for.
    let started = |canceller| {
        let _ = tx.send_blocking(BuildMsg::Started(canceller));
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if clean {
            match Build::clean(request, started) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    return BuildOutcome::cancelled();
                }
                Err(e) => return spawn_failed(&e),
            }
        }
        match Build::start(request) {
            Ok(build) => {
                started(build.canceller());
                build.wait()
            }
            Err(e) => spawn_failed(&e),
        }
    }))
    .unwrap_or_else(|_| BuildOutcome::spawn_failed(&io::Error::other("the build thread panicked")));
    let problems = diagnostics_panel::problems_from(
        &outcome.report.diagnostics,
        &request.root,
        &request.outdir,
    );
    let _ = tx.send_blocking(BuildMsg::Finished(Box::new(Finished { outcome, problems })));
}

/// The outcome of a build whose latexmk did not start. A program that does not exist (latexmk,
/// or the interpreter of its `#!` line) gets [`LATEXMK_MISSING`] instead of the system's "No
/// such file or directory", which says neither what is missing nor how to get it.
fn spawn_failed(error: &io::Error) -> BuildOutcome {
    if error.kind() == io::ErrorKind::NotFound {
        BuildOutcome::spawn_failed(&io::Error::new(error.kind(), LATEXMK_MISSING))
    } else {
        BuildOutcome::spawn_failed(error)
    }
}

/// Root detection input, copied from the main thread.
struct DetectInput {
    /// The active document.
    file: PathBuf,
    text: String,
    /// Every document of the workspace (open or closed) with its current text.
    open: Vec<(PathBuf, String)>,
    /// The root of the last build.
    remembered: Option<PathBuf>,
    /// The user's earlier answer for the file's directory (or one above it).
    chosen: Option<PathBuf>,
    /// The user's answer to "which is the main document?".
    forced_root: Option<PathBuf>,
}

/// What the jobs thread found out.
struct Detection {
    root: Option<PathBuf>,
    root_exists: bool,
    engine: Engine,
    mirror_dirs: Vec<PathBuf>,
}

/// Finds the root, engine and `\include` directories (jobs thread: reads files).
fn detect(input: DetectInput) -> Detection {
    let mut index = ProjectIndex::default();
    for (path, text) in &input.open {
        index.update_file(path.clone(), Arc::new(index_file(text)));
    }
    let guess = match &input.forced_root {
        Some(root) => RootGuess {
            root: Some(root.clone()),
            source: RootSource::Remembered,
        },
        None => {
            let own = index
                .files
                .get(&input.file)
                .cloned()
                .unwrap_or_else(|| Arc::new(index_file(&input.text)));
            let standalone = own.magic.root.is_some()
                || own.magic.subfiles_parent.is_some()
                || (own.has_documentclass && own.has_begin_document);
            if !standalone {
                index_nearby(&input.file, &mut index);
            }
            let remembered = input.chosen.as_deref().or(input.remembered.as_deref());
            let guess = detect_root(&input.file, &input.text, &index, remembered);
            match (&guess.root, &input.chosen) {
                // The user chose this root for the file's directory, wherever the root lives.
                (None, Some(chosen)) => RootGuess {
                    root: Some(chosen.clone()),
                    source: RootSource::Remembered,
                },
                _ => guess,
            }
        }
    };
    let Some(root) = guess.root else {
        return Detection {
            root: None,
            root_exists: false,
            engine: Engine::PdfLatex,
            mirror_dirs: Vec::new(),
        };
    };
    index_project(&root, &mut index);
    // A root that is not open is read from disk: lossily (a Latin-1 document is as good as a
    // UTF-8 one for finding packages and magic comments) and only its first MiB (the preamble).
    let root_text = input
        .open
        .iter()
        .find(|(path, _)| *path == root)
        .map(|(_, text)| text.clone())
        .or_else(|| read_text(&root, MAX_INDEXED_BYTES).map(|(text, _)| text));
    let root_dir = root.parent().unwrap_or(Path::new("/"));
    Detection {
        root_exists: root_text.is_some(),
        engine: detect_engine(root_text.as_deref().unwrap_or(""), root_dir),
        mirror_dirs: mirror_dirs(&index, &root),
        root: Some(root),
    }
}

/// Indexes the `.tex` files in `file`'s directory and up to [`ROOT_SEARCH_LEVELS`] directories
/// above it (never above the home directory), so the include graph can name a root.
fn index_nearby(file: &Path, index: &mut ProjectIndex) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let Some(dir) = file.parent() else {
        return;
    };
    let mut budget = MAX_INDEXED_FILES;
    for dir in dir.ancestors().take(ROOT_SEARCH_LEVELS + 1) {
        if dir.parent().is_none()
            || home
                .as_deref()
                .is_some_and(|h| h.starts_with(dir) && h != dir)
        {
            break;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if budget == 0 {
                return;
            }
            if path.extension().is_some_and(|e| e == "tex") && !index.files.contains_key(&path) {
                budget -= 1;
                index.update_file(path.clone(), Arc::new(read_index(&path)));
            }
        }
        if home.as_deref() == Some(dir) {
            break;
        }
    }
}

/// Indexes every file reachable from `root` through `\input`, `\include`, `\subfile` and
/// `\import` that is not indexed yet.
fn index_project(root: &Path, index: &mut ProjectIndex) {
    let mut budget = MAX_INDEXED_FILES;
    loop {
        let missing: Vec<PathBuf> = index
            .project_files(root)
            .into_iter()
            .filter(|path| !index.files.contains_key(path))
            .collect();
        if missing.is_empty() || budget == 0 {
            return;
        }
        for path in missing.into_iter().take(budget) {
            budget -= 1;
            let file_index = read_index(&path);
            index.update_file(path, Arc::new(file_index));
        }
    }
}

/// The index of the file at `path`; empty when it is not a regular file, cannot be read or is
/// larger than [`MAX_INDEXED_BYTES`].
fn read_index(path: &Path) -> FileIndex {
    match read_text(path, MAX_INDEXED_BYTES) {
        Some((text, true)) => index_file(&text),
        _ => FileIndex::default(),
    }
}

/// The text of the regular file at `path` (symlinks followed), at most `cap` bytes, with invalid
/// UTF-8 replaced: detection only looks for ASCII commands. The flag says whether that was the
/// whole file. `None` for anything but a regular file: a FIFO would block and a device such as
/// /dev/zero never ends.
fn read_text(path: &Path, cap: u64) -> Option<(String, bool)> {
    // Not blocking: opening a FIFO for reading would otherwise wait for a writer.
    let file = crate::fsutil::open_nonblocking(path).ok()?;
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    let complete = bytes.len() as u64 <= cap;
    bytes.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
    Some((String::from_utf8_lossy(&bytes).into_owned(), complete))
}

/// The directories of `\include`d files (relative to the root's directory), which TeX needs to
/// exist under the output directory.
fn mirror_dirs(index: &ProjectIndex, root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for file in index.project_files(root) {
        let Some(file_index) = index.files.get(&file) else {
            continue;
        };
        for include in &file_index.includes {
            if include.kind != IncludeKind::Include {
                continue;
            }
            let Some(dir) = include_dir(include.target.trim()) else {
                continue;
            };
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// The directory of an `\include` target, relative to the root's directory and without `.`
/// components (`./chapters/intro` → `chapters`). `None` for a target in the root's directory,
/// and for absolute targets or targets with `..`, which would leave the output directory.
fn include_dir(target: &str) -> Option<PathBuf> {
    let mut dir = PathBuf::new();
    for component in Path::new(target).parent()?.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => dir.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!dir.as_os_str().is_empty()).then_some(dir)
}

/// The PDF of an earlier build of `root`: `<dir>/<outdir>/<stem>.pdf`, else `<dir>/<stem>.pdf`.
fn existing_pdf(root: &Path, outdir: &str) -> Option<PathBuf> {
    let dir = root.parent()?;
    let name = root.with_extension("pdf");
    let name = name.file_name()?;
    [dir.join(outdir).join(name), dir.join(name)]
        .into_iter()
        .find(|p| p.is_file())
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn build_indicator() -> Indicator {
    let spinner = adw::Spinner::new();
    let icon = gtk::Image::new();
    let errors = gtk::Label::builder()
        .css_classes(["error", "numeric", "heading"])
        .build();
    let warning_icon = gtk::Image::builder()
        .icon_name("dialog-warning-symbolic")
        .css_classes(["warning"])
        .build();
    let warnings = gtk::Label::builder()
        .css_classes(["warning", "numeric"])
        .build();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    for widget in [
        spinner.upcast_ref::<gtk::Widget>(),
        icon.upcast_ref(),
        errors.upcast_ref(),
        warning_icon.upcast_ref(),
        warnings.upcast_ref(),
    ] {
        content.append(widget);
    }
    let button = gtk::Button::builder()
        .child(&content)
        .visible(false)
        .css_classes(["flat", "lemma-build-indicator"])
        .build();
    Indicator {
        button,
        spinner,
        icon,
        errors,
        warning_icon,
        warnings,
    }
}

/// Called once per window after the layout is built: the build indicator, the `win.stop-build`
/// and `win.clean-build` actions, the Problems panel, and loading an earlier build's PDF when a
/// document is shown.
pub fn install(state: &Rc<WindowState>) -> BuildController {
    install_css_once("build", CSS);
    install_css_once("problems", diagnostics_panel::CSS);
    let indicator = build_indicator();
    state
        .header
        .build_indicator
        .set_child(Some(&indicator.button));
    let inner = Rc::new(Inner {
        state: Rc::downgrade(state),
        queue: RefCell::new(BuildQueue::default()),
        preparing: Cell::new(false),
        again: Cell::new(None),
        stop_generation: Cell::new(0),
        waiting_reason: Cell::new(Reason::Manual),
        waiting_journal: RefCell::new(None),
        waiting_clean: Cell::new(false),
        running: RefCell::new(None),
        remembered_root: RefCell::new(None),
        chosen_roots: RefCell::new(HashMap::new()),
        last: RefCell::new(None),
        finished: RefCell::new(Vec::new()),
        toast: RefCell::new(None),
        clean_next: Cell::new(false),
        latexmk_missing_told: Cell::new(false),
        diagnostics: DiagnosticsPanel::new(state),
        indicator,
    });

    let weak = Rc::downgrade(state);
    inner.indicator.button.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            diagnostics_panel::reveal(&state);
        }
    });

    let stop = gio::SimpleAction::new("stop-build", None);
    let weak = Rc::downgrade(&inner);
    stop.connect_activate(move |_, _| {
        if let Some(inner) = weak.upgrade() {
            inner.stop();
        }
    });
    state.window.add_action(&stop);
    let clean = gio::SimpleAction::new("clean-build", None);
    let weak = Rc::downgrade(&inner);
    clean.connect_activate(move |_, _| {
        if let Some(inner) = weak.upgrade() {
            inner.clean_and_rebuild();
        }
    });
    state.window.add_action(&clean);
    set_default_accels(state, ACCELS);

    // Show the PDF of an earlier build as soon as a document is shown and no PDF is.
    let weak = Rc::downgrade(&inner);
    state.tab_view.connect_selected_page_notify(move |_| {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let has_pdf = inner
            .state
            .upgrade()
            .and_then(|s| s.pdf.get().map(|p| p.requested_pdf().is_some()))
            .unwrap_or(true);
        if !has_pdf {
            glib::spawn_future_local(async move {
                if let Err(e) = inner.show_existing_pdf().await {
                    tracing::debug!("no earlier PDF to show: {e}");
                }
            });
        }
    });

    // A closed window stops its build: latexmk runs in its own process group and would
    // otherwise outlive the editor.
    let weak = Rc::downgrade(&inner);
    state.window.connect_destroy(move |_| {
        if let Some(inner) = weak.upgrade() {
            inner.stop();
        }
    });
    BuildController { inner }
}

/// Adds `css` to the display's styles, once per process for each `id` (windows share the
/// display).
pub(crate) fn install_css_once(id: &'static str, css: &'static str) {
    thread_local! {
        static INSTALLED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }
    if INSTALLED.with_borrow(|ids| ids.contains(&id)) {
        return;
    }
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    INSTALLED.with_borrow_mut(|ids| ids.push(id));
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Sets the default accelerators of actions the user did not rebind in `[keys]`. An empty
/// override (no chord) and a valid one were applied by `app::set_accels`; an invalid one is
/// ignored, as there, so the default stays.
pub(crate) fn set_default_accels(state: &WindowState, accels: &[(&str, &[&str])]) {
    crate::app::set_default_accels(state, accels);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lemma_build::LogReport;

    fn outcome(status: BuildStatus, ms: u64) -> BuildOutcome {
        BuildOutcome {
            status,
            exit_code: None,
            elapsed: Duration::from_millis(ms),
            report: LogReport::default(),
            pdf: None,
            pdf_stamp: None,
            synctex: None,
        }
    }

    #[test]
    fn status_texts() {
        assert_eq!(
            status_text(&outcome(BuildStatus::Success, 1234), 0, 0),
            "Built in 1.2 s"
        );
        assert_eq!(
            status_text(&outcome(BuildStatus::Success, 800), 0, 2),
            "Built in 0.8 s · 2 warnings"
        );
        assert_eq!(
            status_text(&outcome(BuildStatus::Errors, 800), 2, 1),
            "Build failed: 2 errors · 1 warning"
        );
        assert_eq!(
            status_text(&outcome(BuildStatus::Errors, 800), 0, 0),
            "Build failed"
        );
        assert_eq!(
            status_text(&outcome(BuildStatus::Cancelled, 800), 0, 0),
            "Build stopped"
        );
        assert_eq!(
            status_text(
                &outcome(BuildStatus::SpawnFailed("permission denied".into()), 0),
                0,
                0
            ),
            "Build could not start"
        );
    }

    /// A latexmk that does not exist is explained, whichever way it is missing; other failures
    /// keep the system's message.
    #[test]
    fn a_missing_latexmk_is_explained() {
        let missing = spawn_failed(&io::Error::new(
            io::ErrorKind::NotFound,
            "cannot run latexmk: No such file or directory (os error 2)",
        ));
        assert_eq!(
            missing.status,
            BuildStatus::SpawnFailed(LATEXMK_MISSING.to_owned())
        );
        assert!(latexmk_missing(&missing));
        assert_eq!(status_text(&missing, 0, 0), "Not built: latexmk not found");
        for kind in ["TeX Live", "MacTeX", "MiKTeX", "Perl", "[build] latexmk"] {
            assert!(LATEXMK_MISSING.contains(kind), "{kind}");
        }
        let denied = spawn_failed(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cannot run latexmk: Permission denied (os error 13)",
        ));
        assert!(!latexmk_missing(&denied));
        assert_eq!(
            denied.status,
            BuildStatus::SpawnFailed(
                "cannot run latexmk: Permission denied (os error 13)".to_owned()
            )
        );
    }

    #[test]
    fn detection_finds_the_root_engine_and_include_dirs() {
        let dir = std::env::temp_dir().join(format!("lemma-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("chapters")).unwrap();
        let dir = lemma_core::canonicalize(&dir).unwrap();
        let main = "\\documentclass{book}\n\\usepackage{fontspec}\n\\begin{document}\n\
                    \\include{chapters/one}\n\\input{sec}\n\\end{document}\n";
        std::fs::write(dir.join("main.tex"), main).unwrap();
        std::fs::write(dir.join("sec.tex"), "\\include{appendix/two}\n").unwrap();
        let chapter = dir.join("chapters/one.tex");
        std::fs::write(&chapter, "Some text.\n").unwrap();

        // A chapter finds its root through the include graph of the parent directory.
        let detection = detect(DetectInput {
            file: chapter.clone(),
            text: "Some text.\n".into(),
            open: vec![(chapter.clone(), "Some text.\n".into())],
            remembered: None,
            chosen: None,
            forced_root: None,
        });
        assert_eq!(
            detection.root.as_deref(),
            Some(dir.join("main.tex").as_path())
        );
        assert!(detection.root_exists);
        assert_eq!(detection.engine, Engine::LuaLatex);
        assert_eq!(
            detection.mirror_dirs,
            vec![PathBuf::from("chapters"), PathBuf::from("appendix")]
        );

        // A file nothing includes has no root unless the user chose one.
        let lone = dir.join("chapters/lone.tex");
        let input = |forced_root| DetectInput {
            file: lone.clone(),
            text: "x\n".into(),
            open: Vec::new(),
            remembered: None,
            chosen: None,
            forced_root,
        };
        assert_eq!(detect(input(None)).root, None);
        let forced = detect(input(Some(dir.join("main.tex"))));
        assert_eq!(forced.root.as_deref(), Some(dir.join("main.tex").as_path()));

        // The PDF of an earlier build: the output directory wins over the root's directory.
        assert_eq!(existing_pdf(&dir.join("main.tex"), ".build"), None);
        std::fs::write(dir.join("main.pdf"), "%PDF").unwrap();
        assert_eq!(
            existing_pdf(&dir.join("main.tex"), ".build"),
            Some(dir.join("main.pdf"))
        );
        std::fs::create_dir_all(dir.join(".build")).unwrap();
        std::fs::write(dir.join(".build/main.pdf"), "%PDF").unwrap();
        assert_eq!(
            existing_pdf(&dir.join("main.tex"), ".build"),
            Some(dir.join(".build/main.pdf"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_dirs_drop_dot_components_and_refuse_escapes() {
        assert_eq!(
            include_dir("chapters/intro"),
            Some(PathBuf::from("chapters"))
        );
        assert_eq!(
            include_dir("./chapters/intro"),
            Some(PathBuf::from("chapters"))
        );
        assert_eq!(
            include_dir("./parts/./one/x"),
            Some(PathBuf::from("parts/one"))
        );
        assert_eq!(include_dir("intro"), None);
        assert_eq!(include_dir("./intro"), None);
        assert_eq!(include_dir("../shared/x"), None);
        assert_eq!(include_dir("/abs/x"), None);
    }

    #[test]
    fn detection_reads_only_regular_files_lossily_and_bounded() {
        let dir = std::env::temp_dir().join(format!("lemma-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let latin1 = dir.join("latin1.tex");
        std::fs::write(&latin1, b"\\input{caf\xe9}\n").unwrap();
        let (text, complete) = read_text(&latin1, 1024).unwrap();
        assert!(complete);
        assert!(text.starts_with("\\input{caf"), "{text:?}");
        assert_eq!(index_file(&text).includes.len(), 1);

        let big = dir.join("big.tex");
        std::fs::write(&big, vec![b'x'; 100]).unwrap();
        assert_eq!(
            read_text(&big, 100).map(|(t, c)| (t.len(), c)),
            Some((100, true))
        );
        assert_eq!(
            read_text(&big, 99).map(|(t, c)| (t.len(), c)),
            Some((99, false))
        );

        // Not regular files: never read (a FIFO would block, /dev/zero never ends).
        #[cfg(unix)]
        {
            let fifo = dir.join("fifo.tex");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap();
            assert!(made.success());
            assert_eq!(read_text(&fifo, 1024), None);
            let zero = dir.join("zero.tex");
            std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
            assert_eq!(read_text(&zero, 1024), None);
            assert_eq!(read_index(&zero), FileIndex::default());
        }
        assert_eq!(read_text(&dir, 1024), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chosen_root_applies_below_its_directory_only() {
        let dir = std::env::temp_dir().join(format!("lemma-chosen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["a/notes", "b"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let dir = lemma_core::canonicalize(&dir).unwrap();
        let root = dir.join("b/thesis.tex");
        std::fs::write(
            &root,
            "\\documentclass{book}\n\\begin{document}\n\\end{document}\n",
        )
        .unwrap();
        let notes = dir.join("a/notes/x.tex");
        std::fs::write(&notes, "Notes.\n").unwrap();
        let input = |remembered: Option<&Path>, chosen: Option<&Path>| DetectInput {
            file: notes.clone(),
            text: "Notes.\n".into(),
            open: Vec::new(),
            remembered: remembered.map(Path::to_path_buf),
            chosen: chosen.map(Path::to_path_buf),
            forced_root: None,
        };
        // The last build's root belongs to another directory tree: not used.
        assert_eq!(detect(input(Some(&root), None)).root, None);
        // The user's answer for this directory is, wherever the root lives.
        let detection = detect(input(None, Some(&root)));
        assert_eq!(detection.root.as_deref(), Some(root.as_path()));
        assert!(detection.root_exists);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn earlier_pdf_texts() {
        // Today: the time only, "PDF from HH:MM".
        let today = earlier_pdf_text(Some(SystemTime::now()));
        let time = today.strip_prefix("PDF from ").unwrap_or_default();
        let shape: String = time
            .chars()
            .map(|c| if c.is_ascii_digit() { '9' } else { c })
            .collect();
        assert_eq!(shape, "99:99", "{today}");
        let old = earlier_pdf_text(Some(
            SystemTime::UNIX_EPOCH + Duration::from_secs(86_400 * 400),
        ));
        assert!(
            old.starts_with("PDF from ") && old.ends_with("1971"),
            "{old}"
        );
        assert_eq!(earlier_pdf_text(None), "PDF from an earlier build");
    }
}
