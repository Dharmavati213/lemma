//! `--demo pdf`: shows the PDF of the open document's last build, fills the page cache as
//! scrolling through the whole document would (the `pdf50` memory scenario of
//! `scripts/budgets.toml`), then zooms out so three whole pages are on screen.

use super::wait_until;
use crate::pdf::pdf_view::{PAGE_GAP, PdfView};
use crate::window::WindowState;
use gtk::prelude::*;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// How long the PDF may take to open and get its first allocation.
const LOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long filling the cache may take.
const FILL_TIMEOUT: Duration = Duration::from_secs(15);
/// How many pages the demo shows at the end.
const SHOWN_PAGES: u32 = 3;

/// Runs the PDF demo; resolves when three pages are on screen at their final render scale.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    let build = state.build.get().ok_or("there is no build controller")?;
    let controller = state.pdf.get().ok_or("there is no PDF preview")?;
    let pdf = build
        .show_existing_pdf()
        .await
        .map_err(|e| format!("no PDF to show ({e}); build the document first"))?;
    let view = controller.view();
    wait_until(LOAD_TIMEOUT, "the PDF to open", || {
        controller.pdf().as_deref() == Some(pdf.as_path())
            && view.page_count() > 0
            && view.render_scale_q() > 0
    })
    .await?;
    // Let the window settle into its first size before rendering everything.
    super::wait_for_frames(&state.window, 1).await;

    fill_cache(view).await?;
    let filled = (view.cache_used(), view.page_count());

    let layout = view.layout().ok_or("the PDF has no layout")?;
    let page_height = layout.page_size(0).map_or(842.0, |s| s.1);
    let height = f64::from(view.height());
    // Three pages and the gaps around them fill the height, with half a gap to spare.
    let gaps = (f64::from(SHOWN_PAGES) + 0.5) * PAGE_GAP;
    view.set_zoom(
        (height - gaps) / (f64::from(SHOWN_PAGES) * page_height),
        None,
    );
    view.set_scroll_position(0.0, 0.0);
    let shown = 0..SHOWN_PAGES.min(view.page_count());
    wait_until(LOAD_TIMEOUT, "three pages to render", || {
        let visible = view.visible_pages();
        visible.start == 0
            && visible.end >= shown.end
            && view.render_scale_q() == lemma_pdf::scale_q(view.zoom(), view.device_scale())
            && shown.clone().all(|p| view.is_page_rendered(p))
    })
    .await?;
    println!(
        "demo pdf: {} ({} pages); cache filled to {:.1} MiB of {:.0} MiB; showing pages 1–{} at \
         {:.0}% (render scale {}/8, surface scale {})",
        pdf.display(),
        filled.1,
        filled.0 as f64 / f64::from(1 << 20),
        view.cache_cap() as f64 / f64::from(1 << 20),
        shown.end,
        view.zoom() * 100.0,
        view.render_scale_q(),
        view.device_scale(),
    );
    Ok(())
}

/// Renders every page at the current scale, until all are cached or the cache cannot hold
/// another page.
async fn fill_cache(view: &PdfView) -> Result<(), String> {
    let deadline = Instant::now() + FILL_TIMEOUT;
    let count = view.page_count();
    loop {
        let all_cached = (0..count).all(|p| view.is_page_rendered(p));
        let page_bytes = view.cache_used() / view_pages_cached(view).max(1);
        let full = view.cache_used() + page_bytes > view.cache_cap();
        if all_cached || (full && view.outstanding_requests() == 0) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "the page cache did not fill within {FILL_TIMEOUT:?} ({} bytes)",
                view.cache_used()
            ));
        }
        if view.outstanding_requests() == 0 {
            view.prefetch(0..count);
        }
        glib::timeout_future(Duration::from_millis(10)).await;
    }
}

fn view_pages_cached(view: &PdfView) -> usize {
    (0..view.page_count())
        .filter(|p| view.is_page_rendered(*p))
        .count()
}
