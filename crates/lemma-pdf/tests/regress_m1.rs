//! Regression tests for PDF worker defects found in the M1 review (area C).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use async_channel::{Receiver, TryRecvError};
use lemma_pdf::{PageKey, PdfCmd, PdfEvent, RenderReq, page_pixels, tile_grid};

const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(rel)
        .canonicalize()
        .unwrap_or_else(|e| panic!("fixture {rel}: {e}"))
}

fn next(rx: &Receiver<PdfEvent>) -> PdfEvent {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match rx.try_recv() {
            Ok(event) => return event,
            Err(TryRecvError::Empty) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_micros(200));
            }
            Err(e) => panic!("no event from the pdf thread: {e:?}"),
        }
    }
}

/// A fresh directory for one test (canonical, so recorded SyncTeX paths match).
fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-pdf-regress-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// Set in the child process of [`regress_worker_shutdown_races_process_exit`].
const EXIT_RACE_CHILD: &str = "LEMMA_PDF_REGRESS_EXIT_RACE_CHILD";

/// C4: every test using `lemma_pdf::spawn` ends with `send(Shutdown)` and returns, and the
/// process exits while the "pdf" thread is still dropping its poppler document. Freeing the
/// last document tears down poppler's `GlobalParams` on that thread while `exit()` destroys the
/// same global, which aborts or segfaults (`cargo test -p lemma-pdf` crashed after "test result:
/// ok" under load).
///
/// The child process does exactly what the worker tests do (open, Shutdown, drop the handle,
/// end); the process must exit cleanly every time. Once `PdfHandle` can wait for its thread
/// (e.g. the last handle's drop joins it, or a `shutdown_and_join`), the child must use that.
#[test]
fn regress_worker_shutdown_races_process_exit() {
    if let Some(pdf) = std::env::var_os(EXIT_RACE_CHILD) {
        let (tx, rx) = async_channel::unbounded();
        let handle = lemma_pdf::spawn(tx);
        handle.send(PdfCmd::Open {
            generation: 1,
            pdf: PathBuf::from(pdf),
        });
        assert!(matches!(next(&rx), PdfEvent::Opened { .. }));
        handle.send(PdfCmd::Shutdown);
        drop(handle);
        // What a test binary does after its last test returns.
        std::process::exit(0);
    }
    let exe = std::env::current_exe().unwrap();
    let pdf = fixture("pdf/stress.pdf");
    const RUNS: usize = 120;
    for run in 1..=RUNS {
        let output = Command::new(&exe)
            .args([
                "--exact",
                "regress_worker_shutdown_races_process_exit",
                "--test-threads=1",
                "--quiet",
            ])
            .env(EXIT_RACE_CHILD, &pdf)
            .output()
            .expect("run the child");
        assert!(
            output.status.success(),
            "run {run}/{RUNS}: the process crashed at exit while the pdf thread freed its \
             document: {:?}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Writes a one-page PDF (`/MediaBox [0 0 612 792]`, `/Rotate 90`) whose content is a black
/// 300×20 bp rectangle at x 72..372, y 172..192 (top-left origin, unrotated page space), and a
/// matching `.synctex` file: line 5 of `main.tex` is an hbox exactly over the rectangle, line 9
/// an hbox at x 400..600, y 212..232.
fn write_rotated_pdf(dir: &Path) -> PathBuf {
    let content = "0 g 72 600 300 20 re f\n";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate 90 \
         /Resources << >> /Contents 4 0 R >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        write!(pdf, "{} 0 obj\n{body}\nendobj\n", i + 1).unwrap();
    }
    let xref = pdf.len();
    writeln!(pdf, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
        objects.len() + 1
    )
    .unwrap();
    let path = dir.join("main.pdf");
    std::fs::write(&path, pdf).unwrap();
    std::fs::write(dir.join("main.tex"), "%\n").unwrap();

    const SP_PER_BP: f64 = 65781.76;
    let sp = |bp: f64| (bp * SP_PER_BP).round() as i64;
    let mut s = String::from("SyncTeX Version:1\n");
    writeln!(s, "Input:1:{}/./main.tex", dir.display()).unwrap();
    s.push_str("Output:pdf\nMagnification:1000\nUnit:1\nX Offset:0\nY Offset:0\nContent:\n{1\n");
    writeln!(
        s,
        "[1,1:{},{}:{},{},0",
        sp(72.0),
        sp(720.0),
        sp(540.0),
        sp(648.0)
    )
    .unwrap();
    // (line, h, baseline, width): height 18 above the baseline, depth 2 below it.
    for (line, h, v, w) in [(5, 72.0, 190.0, 300.0), (9, 400.0, 230.0, 200.0)] {
        writeln!(
            s,
            "(1,{line}:{},{}:{},{},{}",
            sp(h),
            sp(v),
            sp(w),
            sp(18.0),
            sp(2.0)
        )
        .unwrap();
        writeln!(s, "g1,{line}:{},{}\n)", sp(h), sp(v)).unwrap();
    }
    s.push_str("]\n}1\nPostamble:\nCount:1\nPost scriptum:\n");
    std::fs::write(dir.join("main.synctex"), s).unwrap();
    path
}

/// C9: on a page with `/Rotate 90` whose content is not rotated, poppler reports and renders
/// the page rotated (792×612), but SyncTeX boxes and edit queries are in the unrotated page
/// space. Forward boxes must land on the line's ink in the page the view shows, and an inverse
/// query at that ink must give the line.
///
/// (pdflscape pages are a different case: SyncTeX records their content without the
/// `\rotatebox`, so their boxes are already in display orientation but offset; see the review.)
#[test]
fn regress_synctex_ignores_page_rotation() {
    let dir = temp_dir("rotate");
    let pdf = write_rotated_pdf(&dir);
    let (tx, rx) = async_channel::unbounded();
    let worker = lemma_pdf::spawn(tx);
    worker.send(PdfCmd::Open { generation: 1, pdf });
    let sizes = match next(&rx) {
        PdfEvent::Opened { page_sizes, .. } => page_sizes,
        other => panic!("expected Opened, got {other:?}"),
    };
    assert_eq!(sizes, [(792.0, 612.0)], "poppler applies /Rotate");
    worker.send(PdfCmd::Want(vec![RenderReq {
        key: PageKey {
            generation: 1,
            page: 0,
            scale_q: 8,
            tile: None,
        },
        dark: false,
    }]));
    let page = match next(&rx) {
        PdfEvent::Rendered(r) => r,
        other => panic!("expected Rendered, got {other:?}"),
    };
    assert_eq!((page.width, page.height), (792, 612));
    let ink = |x: f64, y: f64| {
        let (x, y) = (x as i32, y as i32);
        x >= 0
            && y >= 0
            && x < page.width
            && y < page.height
            && page.data[(y * page.stride + x * 4) as usize] < 128
    };
    // The rectangle is displayed at x 600..620, y 72..372.
    assert!(ink(610.0, 222.0) && !ink(222.0, 182.0));

    worker.send(PdfCmd::Forward {
        file: dir.join("main.tex"),
        line: 5,
    });
    let boxes = match next(&rx) {
        PdfEvent::Forward { boxes, .. } => boxes,
        other => panic!("expected Forward, got {other:?}"),
    };
    let b = boxes.first().expect("line 5 has a box");
    let (cx, cy) = (
        f64::from(b.h) + f64::from(b.width) / 2.0,
        f64::from(b.v_top) + f64::from(b.height) / 2.0,
    );
    let inside = f64::from(b.h) >= -1.0
        && f64::from(b.v_top) >= -1.0
        && f64::from(b.h + b.width) <= 793.0
        && f64::from(b.v_top + b.height) <= 613.0;
    assert!(
        inside && ink(cx, cy),
        "the forward box {b:?} must cover line 5's ink (displayed at x 600..620, y 72..372) \
         on the 792x612 page"
    );

    // Ctrl+click on line 5's ink, in the coordinates of the page as shown.
    worker.send(PdfCmd::Inverse {
        page: 0,
        x_bp: 610.0,
        y_bp: 222.0,
    });
    let target = match next(&rx) {
        PdfEvent::Inverse { target } => target,
        other => panic!("expected Inverse, got {other:?}"),
    };
    assert_eq!(
        target.map(|t| t.line),
        Some(5),
        "inverse search on line 5's ink"
    );
    worker.send(PdfCmd::Shutdown);
}

/// C13: `page_pixels` clamps a side to `i32::MAX`, and `tile_grid` then computes
/// `px + TILE_SIZE - 1` in i32, which overflows (a panic in debug builds) for a page of a few
/// billion bp, as a PDF can declare in its MediaBox.
#[test]
fn regress_tile_grid_overflows_for_huge_pages() {
    assert_eq!(page_pixels((3e9, 100.0), 8), (i32::MAX, 100));
    assert_eq!(tile_grid((3e9, 100.0), 8), Some((u16::MAX, 1)));
}

/// Writes `main.pdf` (one Letter page with `/Rotate rotate`) whose content is the text
/// "Lemma rotated text, line five" in 12 pt Helvetica with its baseline at y = 192 bp from the
/// top of the unturned page (x from 72), and a matching `main.synctex`: line 5 of `main.tex` is
/// an hbox over that text (72..252 × 182..195), line 9 an hbox lower down (baseline 400).
fn write_rotated_text_pdf(dir: &Path, rotate: u32) -> PathBuf {
    let content = "BT /F1 12 Tf 72 600 Td (Lemma rotated text, line five) Tj ET\n";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate {rotate} \
             /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
        ),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        write!(pdf, "{} 0 obj\n{body}\nendobj\n", i + 1).unwrap();
    }
    let xref = pdf.len();
    writeln!(pdf, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
        objects.len() + 1
    )
    .unwrap();
    let path = dir.join("main.pdf");
    std::fs::write(&path, pdf).unwrap();
    std::fs::write(dir.join("main.tex"), "%\n").unwrap();

    const SP_PER_BP: f64 = 65781.76;
    let sp = |bp: f64| (bp * SP_PER_BP).round() as i64;
    let mut s = String::from("SyncTeX Version:1\n");
    writeln!(s, "Input:1:{}/./main.tex", dir.display()).unwrap();
    s.push_str("Output:pdf\nMagnification:1000\nUnit:1\nX Offset:0\nY Offset:0\nContent:\n{1\n");
    writeln!(
        s,
        "[1,1:{},{}:{},{},0",
        sp(72.0),
        sp(720.0),
        sp(468.0),
        sp(648.0)
    )
    .unwrap();
    // (line, h, baseline, width): height 10 above the baseline, depth 3 below it.
    for (line, h, v, w) in [(5, 72.0, 192.0, 180.0), (9, 72.0, 400.0, 180.0)] {
        writeln!(
            s,
            "(1,{line}:{},{}:{},{},{}",
            sp(h),
            sp(v),
            sp(w),
            sp(10.0),
            sp(3.0)
        )
        .unwrap();
        writeln!(s, "g1,{line}:{},{}\n)", sp(h), sp(v)).unwrap();
    }
    s.push_str("]\n}1\nPostamble:\nCount:1\nPost scriptum:\n");
    std::fs::write(dir.join("main.synctex"), s).unwrap();
    path
}

/// C9, both quarter turns: on a `/Rotate 90` or `/Rotate 270` page with unturned text, the
/// forward box of the text's line lies over the text's ink on the page as shown (upright in
/// TeX's space, so tall on the turned page), and an inverse query at that ink gives the line.
/// poppler-glib does not report `/Rotate`; the turn's direction comes from the text.
#[test]
fn turned_text_pages_map_both_ways() {
    for rotate in [90, 270] {
        let dir = temp_dir(&format!("rotate-text-{rotate}"));
        let pdf = write_rotated_text_pdf(&dir, rotate);
        let (tx, rx) = async_channel::unbounded();
        let worker = lemma_pdf::spawn(tx);
        worker.send(PdfCmd::Open { generation: 1, pdf });
        match next(&rx) {
            PdfEvent::Opened { page_sizes, .. } => assert_eq!(page_sizes, [(792.0, 612.0)]),
            other => panic!("expected Opened, got {other:?}"),
        }
        worker.send(PdfCmd::Want(vec![RenderReq {
            key: PageKey {
                generation: 1,
                page: 0,
                scale_q: 8,
                tile: None,
            },
            dark: false,
        }]));
        let page = match next(&rx) {
            PdfEvent::Rendered(r) => r,
            other => panic!("expected Rendered, got {other:?}"),
        };
        let ink = |x: i32, y: i32| {
            x >= 0
                && y >= 0
                && x < page.width
                && y < page.height
                && page.data[(y * page.stride + x * 4) as usize] < 128
        };
        worker.send(PdfCmd::Forward {
            file: dir.join("main.tex"),
            line: 5,
        });
        let b = match next(&rx) {
            PdfEvent::Forward { boxes, .. } => *boxes.first().expect("line 5 has a box"),
            other => panic!("expected Forward, got {other:?}"),
        };
        assert!(
            b.width < b.height && b.h >= 0.0 && b.h + b.width <= 792.0,
            "/Rotate {rotate}: the box {b:?} runs down the turned page"
        );
        let (x0, y0) = (b.h.floor() as i32, b.v_top.floor() as i32);
        let (x1, y1) = (
            (b.h + b.width).ceil() as i32,
            (b.v_top + b.height).ceil() as i32,
        );
        let inked: Vec<(i32, i32)> = (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| ink(x, y))
            .collect();
        let all: usize = (0..page.height)
            .flat_map(|y| (0..page.width).map(move |x| (x, y)))
            .filter(|&(x, y)| ink(x, y))
            .count();
        assert!(
            !inked.is_empty() && inked.len() * 10 >= all * 9,
            "/Rotate {rotate}: the box {b:?} covers {} of {all} ink pixels",
            inked.len()
        );

        let (x, y) = inked[inked.len() / 2];
        worker.send(PdfCmd::Inverse {
            page: 0,
            x_bp: f64::from(x) + 0.5,
            y_bp: f64::from(y) + 0.5,
        });
        match next(&rx) {
            PdfEvent::Inverse { target } => assert_eq!(
                target.map(|t| t.line),
                Some(5),
                "/Rotate {rotate}: inverse search at ({x}, {y})"
            ),
            other => panic!("expected Inverse, got {other:?}"),
        }
        assert!(worker.shutdown_and_join(TIMEOUT), "the pdf thread stops");
    }
}
