//! `--build`: builds the open document's project as F5 does and waits until the result is on
//! screen, so a snapshot shows the preview and the diagnostics.
//!
//! - After a build that wrote a PDF, the run waits until the preview shows the new PDF with its
//!   visible pages rendered (a build that found nothing to do leaves the PDF shown in place).
//! - After TeX errors, the first error is opened at its line, as clicking its row on the
//!   Problems page would (`--sidebar problems` shows that page).
//!
//! TeX errors are the build's result, not a failure of the run. A build that could not start,
//! could not run, timed out or was stopped is.

use super::wait_until;
use crate::build_ui::{Reason, status_text};
use crate::window::WindowState;
use lemma_build::{BuildOutcome, BuildStatus};
use lemma_core::Severity;
use std::rc::Rc;
use std::time::Duration;

/// Extra time on top of the configured build timeout (saving, root detection, log parsing).
const BUILD_SLACK: Duration = Duration::from_secs(30);
/// How long the new PDF may take to render its visible pages, and the first error to open.
const SHOW_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs the build step; resolves when its result is shown.
pub async fn run_build(state: Rc<WindowState>) -> Result<(), String> {
    let build = state.build.get().ok_or("there is no build controller")?;
    let pdf = state.pdf.get();

    let (tx, rx) = async_channel::bounded::<BuildOutcome>(1);
    build.connect_finished(move |outcome| {
        // Only the first build matters; later ones find the channel full.
        let _ = tx.try_send(outcome.clone());
    });
    build.request(Reason::Manual);

    let timeout = Duration::from_secs(state.config.build.timeout_s) + BUILD_SLACK;
    let mut outcome = None;
    wait_until(timeout, "the build to finish", || {
        outcome = rx.try_recv().ok();
        // A request that cannot start (no main document, unsaved files) never finishes.
        outcome.is_some() || !build.is_busy()
    })
    .await?;
    let Some(outcome) = outcome else {
        return Err(format!(
            "the build did not start: {}",
            state.status.build.text()
        ));
    };

    let (errors, warnings, _) = build.diagnostics().counts();
    println!("build: {}", status_text(&outcome, errors, warnings));
    match &outcome.status {
        BuildStatus::Success | BuildStatus::Errors => {}
        BuildStatus::Cancelled => return Err("the build was stopped".into()),
        BuildStatus::TimedOut => return Err("the build timed out".into()),
        BuildStatus::SpawnFailed(e) => return Err(format!("could not run latexmk: {e}")),
    }

    let has_output = outcome.status == BuildStatus::Success || outcome.exit_code == Some(12);
    if let (true, Some(new_pdf), Some(controller)) = (has_output, &outcome.pdf, pdf) {
        let view = controller.view();
        wait_until(SHOW_TIMEOUT, "the new PDF to render", || {
            let visible = view.visible_pages();
            controller.shows_requested()
                && controller.pdf().as_deref() == Some(new_pdf.as_path())
                && !visible.is_empty()
                && visible.into_iter().all(|p| view.is_page_rendered(p))
        })
        .await?;
    }

    if outcome.status == BuildStatus::Errors {
        let problems = build.diagnostics().problems();
        if let Some(first) = problems.iter().position(|p| p.severity == Severity::Error) {
            build.diagnostics().activate(first);
            let (path, line) = (&problems[first].path, problems[first].line);
            wait_until(SHOW_TIMEOUT, "the first error to open", || {
                state.active_editor().is_some_and(|e| {
                    e.document().path() == path
                        && line.is_none_or(|line| e.cursor_position().0 == line)
                })
            })
            .await?;
            let at = line.map_or_else(String::new, |line| format!(":{line}"));
            println!("build: first error at {}{at}", path.display());
        }
    }
    Ok(())
}
