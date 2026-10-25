//! `LEMMA_PERF`: timing measurements on the real display (M5, `scripts/perf-wayland.sh`).
//!
//! `LEMMA_PERF` is a comma-separated list of scenarios. A run with it (normally scripted, with
//! `--quit-after-ms`) measures them once the FILE argument is open, before the scripted steps of
//! [`crate::testmode`], and prints one `[perf] <scenario> key=value …` line per result on
//! stdout. Times are milliseconds.
//!
//! - `startup`: process start (the top of `main`) to the window's first frame (`first_frame_ms`)
//!   and to the first frame after the document is open (`ready_ms`). With `LEMMA_PERF_T0` (wall
//!   clock nanoseconds since the epoch, taken by the caller right before exec), also from there
//!   (`exec_first_frame_ms`, `exec_ready_ms`), which adds loading the shared libraries. The
//!   first-frame line also has the milestones of [`mark`]: GTK initialised (`gtk_init_ms`), the
//!   window built (`window_ms`), presented (`presented_ms`, which includes creating the renderer)
//!   and, when it came first, the documents of the start open (`opened_ms`).
//! - `typing`: types LaTeX into a new paragraph in the middle of the open document, one
//!   character every 35–65 ms, through the text view's `insert-at-cursor` (the path of an input
//!   method's commit, so buffer handlers, highlighting, completion, spell checking, the outline,
//!   the git gutter and every other buffer listener run; the key-press layer of the typing helpers
//!   and Vim does not, `gtk_typing`'s perf tests time it). Per keystroke: `handler` (the insert
//!   itself), `paint` (the frame that shows it, before-paint to after-paint), `work` (both),
//!   `to_frame` (the insert to the end of that frame, including whatever else the main loop ran
//!   and any wait for the compositor's frame callback) and, when the compositor reports
//!   presentation times, `to_screen` (to the frame's presentation); `slowest` lists the three
//!   slowest keystrokes as `index:to_frame`. `LEMMA_PERF_KEYS` sets the number of keystrokes
//!   (default 200).
//! - `pdf-scroll`: runs `--demo pdf`'s set-up (needs the PDF of the FILE argument), then scrolls
//!   at a steady 2400 px/s for 3 s, once at the demo's zoom (three pages high) and once at fit
//!   width (a scale the cache does not hold yet, so pages render while scrolling). Per frame:
//!   `paint` (before-paint to after-paint) and `interval` (between presentations, or between
//!   after-paints without presentation times); `fps` and `missed` (intervals longer than 1.5
//!   refresh periods).
//!
//! `LEMMA_PERF_FULLSCREEN=1` asks for a full-screen window first (the largest frames; the lines
//! print the window's size). The numbers mean something only on a real
//! display: Broadway paints about once a second.

use crate::window::WindowState;
use gtk::gdk;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The environment variable naming the scenarios.
pub const ENV: &str = "LEMMA_PERF";
/// Wall-clock nanoseconds since the epoch at exec (set by the caller).
pub const T0_ENV: &str = "LEMMA_PERF_T0";
/// The number of keystrokes of `typing`.
pub const KEYS_ENV: &str = "LEMMA_PERF_KEYS";
/// `1`: make the window full screen before `typing` and `pdf-scroll` (the largest frames).
pub const FULLSCREEN_ENV: &str = "LEMMA_PERF_FULLSCREEN";

/// How long to wait for any one frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// Background work after the document opens (spell checking, indexing, the git gutter) that
/// `typing` lets finish first.
const SETTLE: Duration = Duration::from_millis(2000);
const SCROLL_SPEED: f64 = 2400.0;
const SCROLL_TIME: Duration = Duration::from_secs(3);
/// Typed in a loop; a newline follows each round.
const SCRIPT: &str = "We have $x_n \\to \\alpha$ by \\cite{Vistoli} and \\ref{eq:cocycle}, so \
                      every map $f\\colon X \\to Y$ is flat. Hence $H^i(X, \\cF) = 0$ for $i > 0$.";

/// One measurement of [`ENV`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Startup,
    Typing,
    PdfScroll,
}

impl Scenario {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "startup" => Self::Startup,
            "typing" => Self::Typing,
            "pdf-scroll" => Self::PdfScroll,
            _ => return None,
        })
    }
}

/// Parses a [`ENV`] value: names separated by commas (blanks ignored).
pub fn parse(value: &str) -> Result<Vec<Scenario>, String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| Scenario::from_name(n).ok_or_else(|| format!("{ENV}: unknown scenario {n:?}")))
        .collect()
}

/// The scenarios asked for (none without [`ENV`]; unknown names are reported by [`run`]).
pub fn requested() -> Vec<Scenario> {
    std::env::var(ENV)
        .ok()
        .and_then(|v| parse(&v).ok())
        .unwrap_or_default()
}

thread_local! {
    /// Start-up milestones noted by [`mark`]: name, time since process start.
    static MARKS: RefCell<Vec<(&'static str, Duration)>> = const { RefCell::new(Vec::new()) };
}

/// Notes a start-up milestone (`<name>_ms` on the `first_frame_ms` line) when `startup` is
/// asked for.
pub fn mark(name: &'static str, started: Instant) {
    if requested().contains(&Scenario::Startup) {
        MARKS.with(|m| m.borrow_mut().push((name, started.elapsed())));
    }
}

/// Prints `first_frame_ms` (and the milestones of [`mark`]) at the window's first frame when
/// `startup` is asked for. Called right after the window is presented.
pub fn watch_first_frame(window: &impl IsA<gtk::Widget>, started: Instant) {
    if !requested().contains(&Scenario::Startup) {
        return;
    }
    let Some(clock) = window.frame_clock() else {
        return;
    };
    let handler: Rc<Cell<Option<glib::SignalHandlerId>>> = Rc::default();
    let slot = handler.clone();
    let id = clock.connect_after_paint(move |clock| {
        let mut line = since_start("first_frame_ms", started);
        MARKS.with(|m| {
            for (name, at) in m.borrow().iter() {
                let _ = write!(line, " {name}_ms={}", ms(*at));
            }
        });
        println!("[perf] startup {line}");
        if let Some(id) = slot.take() {
            clock.disconnect(id);
        }
    });
    handler.set(Some(id));
}

/// `key=ms` from `started`, plus `exec_key=ms` from [`T0_ENV`] when it is set.
fn since_start(key: &str, started: Instant) -> String {
    let mut out = format!("{key}={}", ms(started.elapsed()));
    let t0 = std::env::var(T0_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u128>().ok());
    if let (Some(t0), Ok(now)) = (t0, SystemTime::now().duration_since(UNIX_EPOCH)) {
        let nanos = now.as_nanos().saturating_sub(t0);
        let _ = write!(out, " exec_{key}={:.1}", nanos as f64 / 1e6);
    }
    out
}

/// Runs the scenarios of [`ENV`] (see the module docs). Does nothing without it.
pub async fn run(state: &Rc<WindowState>, started: Instant) -> Result<(), String> {
    let Ok(value) = std::env::var(ENV) else {
        return Ok(());
    };
    let scenarios = parse(&value)?;
    if std::env::var(FULLSCREEN_ENV).is_ok_and(|v| v == "1") {
        state.window.fullscreen();
        glib::timeout_future(Duration::from_millis(300)).await;
        wait_frames(state, 1).await;
    }
    let mut failures = Vec::new();
    for scenario in scenarios {
        let result = match scenario {
            Scenario::Startup => {
                // The frame that shows the open document.
                wait_frames(state, 1).await;
                println!("[perf] startup {}", since_start("ready_ms", started));
                Ok(())
            }
            Scenario::Typing => typing(state).await,
            Scenario::PdfScroll => pdf_scroll(state).await,
        };
        if let Err(e) = result {
            failures.push(format!("{ENV} {scenario:?}: {e}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

async fn wait_frames(state: &WindowState, frames: u32) -> bool {
    crate::testmode::wait_for_frames(&state.window, frames).await
}

/// One painted frame.
#[derive(Clone, Copy, Debug)]
struct Frame {
    start: Instant,
    end: Instant,
    /// `glib::monotonic_time()` at the end, in µs (the clock of presentation times).
    end_us: i64,
    counter: i64,
}

#[derive(Default)]
struct Log {
    started: Cell<Option<Instant>>,
    frames: RefCell<Vec<Frame>>,
    /// Frame counter → presentation time (µs, `glib::monotonic_time()`'s clock).
    presented: RefCell<BTreeMap<i64, i64>>,
    /// The display's refresh period in µs, as GDK reports it.
    refresh_us: Cell<i64>,
}

/// Records the frames of a window's frame clock until dropped.
struct Recorder {
    clock: gdk::FrameClock,
    handlers: Vec<glib::SignalHandlerId>,
    log: Rc<Log>,
    painted: async_channel::Receiver<()>,
}

impl Recorder {
    fn new(widget: &impl IsA<gtk::Widget>) -> Result<Self, String> {
        let clock = widget.frame_clock().ok_or("the window is not realized")?;
        let log = Rc::new(Log::default());
        let (tx, painted) = async_channel::unbounded();
        let l = log.clone();
        let before = clock.connect_before_paint(move |_| l.started.set(Some(Instant::now())));
        let l = log.clone();
        let after = clock.connect_after_paint(move |clock| {
            let end = Instant::now();
            let end_us = glib::monotonic_time();
            let counter = clock.frame_counter();
            let start = l.started.take().unwrap_or(end);
            l.frames.borrow_mut().push(Frame {
                start,
                end,
                end_us,
                counter,
            });
            harvest(clock, &l, counter - 1);
            let _ = tx.try_send(());
        });
        Ok(Self {
            clock,
            handlers: vec![before, after],
            log,
            painted,
        })
    }

    /// Forgets the frames painted so far.
    fn drain(&self) {
        while self.painted.try_recv().is_ok() {}
    }

    /// Waits for the next after-paint; false on a timeout.
    async fn next_frame(&self) -> bool {
        glib::future_with_timeout(FRAME_TIMEOUT, self.painted.recv())
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// The presentation time of frame `counter`, if the compositor reported one.
    fn presented(&self, counter: i64) -> Option<i64> {
        harvest(&self.clock, &self.log, counter);
        self.log.presented.borrow().get(&counter).copied()
    }

    fn refresh_us(&self) -> i64 {
        match self.log.refresh_us.get() {
            0 => 16_667,
            r => r,
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        for id in self.handlers.drain(..) {
            self.clock.disconnect(id);
        }
    }
}

/// Stores the presentation times of the frames up to `last` that GDK still has (it keeps 16).
fn harvest(clock: &gdk::FrameClock, log: &Log, last: i64) {
    let mut presented = log.presented.borrow_mut();
    for counter in (last - 15).max(0)..=last {
        if presented.contains_key(&counter) {
            continue;
        }
        let Some(timings) = clock.timings(counter) else {
            continue;
        };
        if !timings.is_complete() {
            continue;
        }
        if timings.refresh_interval() > 0 {
            log.refresh_us.set(timings.refresh_interval());
        }
        if timings.presentation_time() > 0 {
            presented.insert(counter, timings.presentation_time());
        }
    }
}

/// The window's size, `WxH`.
fn size(state: &WindowState) -> String {
    format!("{}x{}", state.window.width(), state.window.height())
}

fn ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1e3)
}

/// `name_p50=… name_p99=… name_max=…` of `values` (ms); nothing for no values.
pub fn summary(name: &str, mut values: Vec<f64>) -> String {
    if values.is_empty() {
        return String::new();
    }
    values.sort_by(f64::total_cmp);
    let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
    format!(
        " {name}_p50={:.2} {name}_p99={:.2} {name}_max={:.2}",
        at(0.5),
        at(0.99),
        values[values.len() - 1]
    )
}

/// The three slowest of `values` as `index:ms` (the index of the keystroke), slowest first.
fn slowest(values: &[f64]) -> String {
    let mut indexed: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| b.1.total_cmp(&a.1));
    let top: Vec<String> = indexed
        .iter()
        .take(3)
        .map(|(i, v)| format!("{i}:{v:.1}"))
        .collect();
    top.join(",")
}

/// A small deterministic generator for the gaps between keystrokes.
fn next_gap(seed: &mut u64) -> Duration {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    Duration::from_millis(35 + (*seed >> 33) % 31)
}

#[derive(Default)]
struct Key {
    handler: f64,
    paint: f64,
    to_frame: f64,
    /// Insert time in µs, frame counter.
    pending: Option<(i64, i64)>,
    to_screen: Option<f64>,
}

async fn typing(state: &Rc<WindowState>) -> Result<(), String> {
    let editor = state.active_editor().ok_or("no document is open")?;
    let keys = std::env::var(KEYS_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .max(1);
    glib::timeout_future(SETTLE).await;
    let view = editor.view().clone();
    let buffer = editor.buffer().clone();
    let lines = buffer.line_count();
    // A new paragraph after the middle line.
    let Some(mut at) = buffer.iter_at_line(lines / 2) else {
        return Err("the document has no middle line".into());
    };
    if !at.ends_line() {
        at.forward_to_line_end();
    }
    buffer.place_cursor(&at);
    view.emit_insert_at_cursor("\n\n");
    view.scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.5);
    glib::timeout_future(Duration::from_millis(500)).await;
    wait_frames(state, 1).await;

    let recorder = Recorder::new(&state.window)?;
    let mut samples: Vec<Key> = Vec::with_capacity(keys);
    let mut seed = 0x5eed_u64;
    let mut script = SCRIPT.chars().chain(std::iter::once('\n')).cycle();
    let mut missed = 0;
    for _ in 0..keys {
        glib::timeout_future(next_gap(&mut seed)).await;
        resolve_screen(&recorder, &mut samples);
        let c = script.next().unwrap_or(' ');
        let mut text = [0u8; 4];
        recorder.drain();
        let t0 = Instant::now();
        let t0_us = glib::monotonic_time();
        view.emit_insert_at_cursor(c.encode_utf8(&mut text));
        let t1 = Instant::now();
        if !recorder.next_frame().await {
            missed += 1;
            continue;
        }
        let Some(frame) = recorder.log.frames.borrow().last().copied() else {
            continue;
        };
        samples.push(Key {
            handler: (t1 - t0).as_secs_f64() * 1e3,
            paint: (frame.end - frame.start).as_secs_f64() * 1e3,
            to_frame: (frame.end - t0).as_secs_f64() * 1e3,
            pending: Some((t0_us, frame.counter)),
            to_screen: None,
        });
    }
    glib::timeout_future(Duration::from_millis(100)).await;
    resolve_screen(&recorder, &mut samples);
    let file = editor
        .document()
        .path()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let get = |f: fn(&Key) -> f64| samples.iter().map(f).collect::<Vec<_>>();
    let screen: Vec<f64> = samples.iter().filter_map(|k| k.to_screen).collect();
    println!(
        "[perf] typing file={file} lines={lines} window={} keys={} timeouts={missed}{}{}{}{}{} \
         slowest={}",
        size(state),
        samples.len(),
        summary("handler", get(|k| k.handler)),
        summary("paint", get(|k| k.paint)),
        summary("work", get(|k| k.handler + k.paint)),
        summary("to_frame", get(|k| k.to_frame)),
        summary("to_screen", screen),
        slowest(&get(|k| k.to_frame)),
    );
    if samples.is_empty() {
        return Err("no keystroke was painted".into());
    }
    Ok(())
}

/// Fills in `to_screen` for keystrokes whose frame has a presentation time by now.
fn resolve_screen(recorder: &Recorder, samples: &mut [Key]) {
    for key in samples.iter_mut() {
        if let Some((t0_us, counter)) = key.pending
            && let Some(presented) = recorder.presented(counter)
        {
            key.to_screen = Some((presented - t0_us) as f64 / 1e3);
            key.pending = None;
        }
    }
}

async fn pdf_scroll(state: &Rc<WindowState>) -> Result<(), String> {
    crate::testmode::pdf::run_demo(state.clone()).await?;
    let view = state
        .pdf
        .get()
        .ok_or("there is no PDF preview")?
        .view()
        .clone();
    scroll(state, &view, "pages3").await?;
    view.set_fit_width();
    let rendered = || {
        let visible = view.visible_pages();
        view.render_scale_q() == lemma_pdf::scale_q(view.zoom(), view.device_scale())
            && !visible.is_empty()
            && visible.clone().all(|p| view.is_page_rendered(p))
    };
    crate::testmode::wait_until(Duration::from_secs(10), "fit-width pages", rendered).await?;
    scroll(state, &view, "fit-width").await
}

/// Scrolls `view` down at [`SCROLL_SPEED`] for [`SCROLL_TIME`] from its top and prints the frames.
async fn scroll(
    state: &WindowState,
    view: &crate::pdf::pdf_view::PdfView,
    zoom: &str,
) -> Result<(), String> {
    view.set_scroll_position(0.0, 0.0);
    wait_frames(state, 2).await;
    let recorder = Recorder::new(&state.window)?;
    let first: Rc<Cell<Option<i64>>> = Rc::default();
    let done = Rc::new(Cell::new(false));
    let (f, d) = (first.clone(), done.clone());
    let tick = view.add_tick_callback(move |view, clock| {
        let now = clock.frame_time();
        let t0 = *f.get().get_or_insert(now);
        f.set(Some(t0));
        let elapsed = (now - t0) as f64 / 1e6;
        if elapsed >= SCROLL_TIME.as_secs_f64() {
            d.set(true);
            return glib::ControlFlow::Break;
        }
        let x = view.scroll_position().0;
        view.set_scroll_position(x, SCROLL_SPEED * elapsed);
        glib::ControlFlow::Continue
    });
    let deadline = Instant::now() + SCROLL_TIME + FRAME_TIMEOUT;
    while !done.get() {
        if Instant::now() > deadline {
            tick.remove();
            return Err(format!("{zoom}: the scroll did not finish"));
        }
        glib::timeout_future(Duration::from_millis(20)).await;
    }
    glib::timeout_future(Duration::from_millis(100)).await;
    let frames = recorder.log.frames.borrow().clone();
    let paint: Vec<f64> = frames
        .iter()
        .map(|f| (f.end - f.start).as_secs_f64() * 1e3)
        .collect();
    // Presentation times when the compositor reports them, else the ends of the paints. A frame
    // without one was discarded by the compositor (or never reported).
    let mut times: Vec<i64> = frames
        .iter()
        .filter_map(|f| recorder.presented(f.counter))
        .collect();
    let presented = times.len();
    let source = if presented * 10 >= frames.len() * 9 {
        "presented"
    } else {
        times = frames.iter().map(|f| f.end_us).collect();
        "painted"
    };
    let interval: Vec<f64> = times
        .windows(2)
        .map(|w| (w[1] - w[0]) as f64 / 1e3)
        .collect();
    let refresh = recorder.refresh_us() as f64 / 1e3;
    let missed = interval.iter().filter(|&&i| i > refresh * 1.5).count();
    let span = match (times.first(), times.last()) {
        (Some(a), Some(b)) if b > a => (b - a) as f64 / 1e6,
        _ => 0.0,
    };
    let fps = if span > 0.0 {
        (times.len() - 1) as f64 / span
    } else {
        0.0
    };
    println!(
        "[perf] pdf-scroll zoom={zoom} scale={:.0}% window={} pane={}x{} frames={} \
         presented={presented} refresh_ms={refresh:.2} intervals={source} fps={fps:.1} \
         missed={missed}{}{}",
        view.zoom() * 100.0,
        size(state),
        view.width(),
        view.height(),
        frames.len(),
        summary("paint", paint),
        summary("interval", interval),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scenarios() {
        assert_eq!(
            parse("startup, typing,pdf-scroll,").unwrap(),
            vec![Scenario::Startup, Scenario::Typing, Scenario::PdfScroll]
        );
        assert_eq!(parse("").unwrap(), vec![]);
        assert!(parse("typing,scroll").unwrap_err().contains("\"scroll\""));
    }

    #[test]
    fn summaries() {
        assert_eq!(summary("x", vec![]), "");
        let values: Vec<f64> = (1..=100).rev().map(f64::from).collect();
        assert_eq!(
            summary("work", values),
            " work_p50=51.00 work_p99=99.00 work_max=100.00"
        );
    }

    #[test]
    fn slowest_keys() {
        assert_eq!(slowest(&[1.0, 9.5, 3.0, 700.3]), "3:700.3,1:9.5,2:3.0");
        assert_eq!(slowest(&[]), "");
    }

    #[test]
    fn gaps_stay_in_range() {
        let mut seed = 1;
        let gaps: Vec<_> = (0..1000).map(|_| next_gap(&mut seed)).collect();
        assert!(gaps.iter().all(|g| (35..=65).contains(&g.as_millis())));
        assert!(gaps.iter().any(|g| g.as_millis() < 40) && gaps.iter().any(|g| g.as_millis() > 60));
    }
}
