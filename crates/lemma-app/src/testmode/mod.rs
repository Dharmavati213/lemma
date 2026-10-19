//! Scripted runs for tests, screenshots and measurements: `--demo`, `--apply-as-agent`,
//! `--build`, `--sidebar`, `--mem-report`, `--snapshot` and `--quit-after-ms`; and finding the
//! helper binaries (`lemma-mcp`, `lemma-fake-agent`) next to `lemma`.
//!
//! Animations are turned off for scripted runs (see `app.rs`), so a snapshot never catches a
//! transition halfway.
//!
//! Order of a scripted run (after the FILE argument is open):
//!
//! 1. the demo (`--demo`), the agent edits (`--apply-as-agent`) and the build (`--build`) run
//!    to completion, in that order; then the sidebar page of `--sidebar` is shown (the
//!    project's session is restored before all this only with `--restore-session`);
//! 2. a few frames are drawn;
//! 3. with `--quit-after-ms N`, the run waits until N ms after process start (so idle callbacks
//!    and relayouts settle);
//! 4. the memory report is printed (before any snapshot: rendering a texture adds tens of MB);
//! 5. the window is rendered to the PNG;
//! 6. with `--quit-after-ms`, the app quits.
//!
//! Snapshots use a `gsk::CairoRenderer` realized without a surface, so they work on every GDK
//! backend, Broadway included.

pub mod agent;
pub mod build;
pub mod completion;
pub mod pdf;
pub mod review;

use crate::cli::{Demo, Options};
use crate::memreport;
use crate::window::WindowState;
use gtk::prelude::*;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// How long to wait for frames before snapshotting anyway.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs the scripted part of a run (see the module docs). Every step runs even when an earlier
/// one fails; the error lists all failures.
pub async fn run(state: &Rc<WindowState>, opts: &Options, started: Instant) -> Result<(), String> {
    let mut failures = Vec::new();
    if let Some(demo) = opts.demo
        && let Err(e) = run_demo(state, demo).await
    {
        failures.push(format!("--demo {}: {e}", demo.name()));
    }
    if let Some(json) = &opts.apply_as_agent
        && let Err(e) = review::run_apply_as_agent(state.clone(), json.clone()).await
    {
        failures.push(format!("--apply-as-agent: {e}"));
    }
    if opts.build
        && let Err(e) = build::run_build(state.clone()).await
    {
        failures.push(format!("--build: {e}"));
    }
    if let Some(page) = opts.sidebar {
        state.sidebar.show_page(page.name());
    }

    if !wait_for_frames(&state.window, 1).await {
        tracing::warn!("no frames were drawn within {FRAME_TIMEOUT:?}");
    }
    let deadline = opts.quit_after.map(|after| started + after);
    if let Some(deadline) = deadline {
        glib::timeout_future(deadline.saturating_duration_since(Instant::now())).await;
    }
    if opts.mem_report {
        memreport::print_report(&format!("after {} ms", started.elapsed().as_millis()));
    }
    if let Some(png) = &opts.snapshot {
        match snapshot_next_frame(&state.window, png).await {
            Ok((w, h)) => println!("snapshot: {} ({w}x{h})", png.display()),
            Err(e) => failures.push(e),
        }
    }
    if deadline.is_some() {
        state.app.quit();
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

async fn run_demo(state: &Rc<WindowState>, demo: Demo) -> Result<(), String> {
    let state = state.clone();
    match demo {
        Demo::Review => review::run_demo(state).await,
        Demo::Pdf => pdf::run_demo(state).await,
        Demo::Agent => agent::run_demo(state).await,
        Demo::Completion => completion::run_demo(state).await,
        // Each feature module runs its own demo.
        Demo::Outline => crate::outline_panel::run_demo(state).await,
        Demo::HoverRef => crate::labels::run_hover_ref_demo(state).await,
        Demo::MathPreview => crate::editor::math_preview::run_demo(state).await,
        Demo::PdfSearch => crate::pdf::search::run_demo(state).await,
        Demo::Palette => crate::palette::run_demo(state).await,
        Demo::Search => crate::search_panel::run_demo(state).await,
        Demo::Prefs => crate::prefs::run_demo(state).await,
        Demo::Pairs => crate::editor::pairs::run_demo(state).await,
        Demo::Templates => crate::templates::run_demo(state).await,
    }
}

/// The environment variable naming the `lemma-fake-agent` binary (an absolute path).
pub const FAKE_AGENT_ENV: &str = "LEMMA_FAKE_AGENT";

/// The helper binary `name` (`lemma-mcp`, `lemma-fake-agent`) built next to the running
/// executable: in its directory, or in the directory above when the executable is a test in
/// cargo's `deps/` (`target/debug/deps/gtk_x-…` → `target/debug/lemma-mcp`). `None` when it is
/// not there.
pub fn sibling_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    sibling_of(&exe, name)
}

fn sibling_of(exe: &Path, name: &str) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let candidates = [Some(dir.join(name)), dir.parent().map(|d| d.join(name))];
    candidates
        .into_iter()
        .flatten()
        .find(|p| p.is_file() && is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// The `lemma-fake-agent` binary: `$LEMMA_FAKE_AGENT` when set (it must name an executable
/// file), else [`sibling_binary`]. The bridge runs it instead of node and the ACP adapter when a
/// scenario is set (`--agent-scenario`).
pub fn fake_agent_binary() -> Option<PathBuf> {
    match std::env::var_os(FAKE_AGENT_ENV) {
        Some(path) => {
            let path = PathBuf::from(path);
            (path.is_file() && is_executable(&path)).then_some(path)
        }
        None => sibling_binary("lemma-fake-agent"),
    }
}

/// Polls `done` every 10 ms until it returns true; fails after `timeout`, naming `what`.
pub async fn wait_until(
    timeout: Duration,
    what: &str,
    mut done: impl FnMut() -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() > deadline {
            return Err(format!("timed out waiting for {what}"));
        }
        glib::timeout_future(Duration::from_millis(10)).await;
    }
    Ok(())
}

/// Waits until `widget`'s window has painted `frames` more frames. Returns false if that did not
/// happen within a few seconds (or the widget is not realized).
///
/// Under Broadway without a connected browser each frame waits for the daemon (about 1 s), so
/// callers wait for as few frames as they need.
pub async fn wait_for_frames(widget: &impl IsA<gtk::Widget>, frames: u32) -> bool {
    let Some(clock) = widget.frame_clock() else {
        return false;
    };
    let (tx, rx) = async_channel::bounded(1);
    let painted = Cell::new(0);
    let handler = clock.connect_after_paint(move |_| {
        painted.set(painted.get() + 1);
        if painted.get() == frames {
            // The receiver may have timed out already.
            let _ = tx.try_send(());
        }
    });
    widget.queue_draw();
    let done = glib::future_with_timeout(FRAME_TIMEOUT, rx.recv())
        .await
        .is_ok_and(|r| r.is_ok());
    clock.disconnect(handler);
    done
}

/// Saves the next frame of `widget` (normally the window) as a PNG; returns the image size.
///
/// The image is taken inside the frame clock's `after-paint` handler: GTK drops a widget's cached
/// render node whenever a redraw is queued (a blinking cursor is enough), so only right after a
/// paint is the node guaranteed to be there.
pub async fn snapshot_next_frame(
    widget: &impl IsA<gtk::Widget>,
    path: &Path,
) -> Result<(i32, i32), String> {
    let widget = widget.as_ref().clone();
    let clock = widget
        .frame_clock()
        .ok_or("snapshot: the window is not realized")?;
    let (tx, rx) = async_channel::bounded(1);
    let taken = Cell::new(false);
    let (target, path) = (widget.clone(), path.to_owned());
    let handler = clock.connect_after_paint(move |_| {
        if !taken.replace(true) {
            // The receiver may have timed out already.
            let _ = tx.try_send(snapshot_png(&target, &path));
        }
    });
    widget.queue_draw();
    let result = glib::future_with_timeout(FRAME_TIMEOUT, rx.recv()).await;
    clock.disconnect(handler);
    match result {
        Ok(Ok(result)) => result,
        _ => Err(format!(
            "snapshot: no frame was drawn within {FRAME_TIMEOUT:?}"
        )),
    }
}

/// Renders `widget` as it was last drawn and saves it as a PNG; returns the image size. Only
/// valid right after a paint (see [`snapshot_next_frame`]).
pub fn snapshot_png(widget: &impl IsA<gtk::Widget>, path: &Path) -> Result<(i32, i32), String> {
    let widget = widget.as_ref();
    let (width, height) = (widget.width(), widget.height());
    if width <= 0 || height <= 0 {
        return Err("snapshot: the window has no size yet".into());
    }
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    snapshot_popovers(widget, &snapshot);
    let node = snapshot
        .to_node()
        .ok_or("snapshot: the window has not drawn anything")?;
    let renderer = gtk::gsk::CairoRenderer::new();
    renderer
        .realize(None)
        .map_err(|e| format!("snapshot: cannot start the cairo renderer: {e}"))?;
    let viewport = gtk::graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(&node, Some(&viewport));
    renderer.unrealize();
    texture
        .save_to_png(path)
        .map_err(|e| format!("snapshot: cannot write {}: {e}", path.display()))?;
    Ok((width, height))
}

/// Draws the popovers shown inside `widget` (the completion list, the inline-edit popover, the
/// hover popover) at their place: they are surfaces of their own, which `widget`'s paintable
/// leaves out.
fn snapshot_popovers(widget: &gtk::Widget, snapshot: &gtk::Snapshot) {
    let mut stack = vec![widget.clone()];
    while let Some(parent) = stack.pop() {
        let mut child = parent.first_child();
        while let Some(widget_child) = child {
            child = widget_child.next_sibling();
            stack.push(widget_child.clone());
            let Ok(popover) = widget_child.clone().downcast::<gtk::Popover>() else {
                continue;
            };
            if !popover.is_mapped() {
                continue;
            }
            let (w, h) = (popover.width(), popover.height());
            if let Some(origin) = popover_origin(widget, &popover).filter(|_| w > 0 && h > 0) {
                snapshot.save();
                snapshot.translate(&origin);
                gtk::WidgetPaintable::new(Some(&widget_child)).snapshot(
                    snapshot,
                    f64::from(w),
                    f64::from(h),
                );
                snapshot.restore();
            }
        }
    }
}

/// Height of a popover's arrow (GTK 4's `TAIL_HEIGHT`).
const POPOVER_TAIL: f32 = 12.0;

/// Where `popover` is drawn in `widget`'s coordinates: its popup surface's position in the
/// parent surface, plus the popover's offset in its surface, minus `widget`'s offset in its own
/// (client-side shadows) — then moved so that what is visible of it (its contents and arrow)
/// touches its anchor, as a compositor places it. (Broadway places the whole surface, shadow
/// included, against the anchor, so popovers would look a shadow's width away from what they
/// point at.)
fn popover_origin(widget: &gtk::Widget, popover: &gtk::Popover) -> Option<gtk::graphene::Point> {
    let native = popover.native()?;
    let popup = native.surface()?.downcast::<gtk::gdk::Popup>().ok()?;
    let (px, py) = native.surface_transform();
    let (wx, wy) = widget.native()?.surface_transform();
    let mut x = (f64::from(popup.position_x()) + px - wx) as f32;
    let mut y = (f64::from(popup.position_y()) + py - wy) as f32;
    if let (Some(anchor), Some(contents)) = (popover_anchor(widget, popover), popover.first_child())
        && let Some(c) = contents.compute_bounds(popover)
    {
        let tail = if popover.has_arrow() {
            POPOVER_TAIL
        } else {
            0.0
        };
        let (w, h) = (popover.width() as f32, popover.height() as f32);
        let (a_top, a_bottom) = (anchor.y(), anchor.y() + anchor.height());
        let (a_left, a_right) = (anchor.x(), anchor.x() + anchor.width());
        if y + h <= a_top + 1.0 {
            // Above its anchor: the arrow's tip on the anchor's top.
            y = a_top - (c.y() + c.height() + tail);
        } else if y >= a_bottom - 1.0 {
            y = a_bottom - (c.y() - tail);
        } else if x + w <= a_left + 1.0 {
            x = a_left - (c.x() + c.width() + tail);
        } else if x >= a_right - 1.0 {
            x = a_right - (c.x() - tail);
        }
    }
    Some(gtk::graphene::Point::new(x, y))
}

/// The rectangle `popover` points at, in `widget`'s coordinates.
fn popover_anchor(widget: &gtk::Widget, popover: &gtk::Popover) -> Option<gtk::graphene::Rect> {
    let parent = popover.parent()?;
    let (pointing, rect) = popover.pointing_to();
    let rect = if pointing {
        gtk::graphene::Rect::new(
            rect.x() as f32,
            rect.y() as f32,
            rect.width() as f32,
            rect.height() as f32,
        )
    } else {
        gtk::graphene::Rect::new(0.0, 0.0, parent.width() as f32, parent.height() as f32)
    };
    let origin = parent.compute_point(widget, &gtk::graphene::Point::new(rect.x(), rect.y()))?;
    Some(gtk::graphene::Rect::new(
        origin.x(),
        origin.y(),
        rect.width(),
        rect.height(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_binaries_next_to_the_executable_or_above_deps() {
        let dir = std::env::temp_dir().join(format!("lemma-sibling-{}", std::process::id()));
        let deps = dir.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let tool = dir.join("lemma-mcp");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let plain = dir.join("not-executable");
        std::fs::write(&plain, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();

        // `target/debug/lemma` and `target/debug/deps/gtk_test-1234`.
        assert_eq!(
            sibling_of(&dir.join("lemma"), "lemma-mcp"),
            Some(tool.clone())
        );
        assert_eq!(
            sibling_of(&deps.join("gtk_test-1234"), "lemma-mcp"),
            Some(tool)
        );
        assert_eq!(sibling_of(&dir.join("lemma"), "not-executable"), None);
        assert_eq!(sibling_of(&dir.join("lemma"), "missing"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
