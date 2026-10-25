//! The PDF worker against the fixture PDFs: rendering, latest-wins queueing, generations, tiles,
//! links and SyncTeX.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_channel::{Receiver, TryRecvError};
use lemma_pdf::{
    LinkDest, PageKey, PdfCmd, PdfEvent, PdfHandle, RenderReq, Rendered, page_pixels, tile_grid,
    tile_rect, tiles_covering,
};

const LETTER: (f64, f64) = (612.0, 792.0);
const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture(rel: &str) -> PathBuf {
    lemma_core::canonicalize(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(rel),
    )
    .unwrap_or_else(|e| panic!("fixture {rel}: {e}"))
}

fn try_next(rx: &Receiver<PdfEvent>, timeout: Duration) -> Result<PdfEvent, TryRecvError> {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Err(TryRecvError::Empty) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_micros(200));
            }
            other => return other,
        }
    }
}

fn next(rx: &Receiver<PdfEvent>) -> PdfEvent {
    try_next(rx, TIMEOUT).unwrap_or_else(|e| panic!("no event from the pdf thread: {e:?}"))
}

fn rendered(rx: &Receiver<PdfEvent>) -> Rendered {
    match next(rx) {
        PdfEvent::Rendered(r) => r,
        other => panic!("expected Rendered, got {other:?}"),
    }
}

fn key(generation: u64, page: u32, scale_q: u16) -> PageKey {
    PageKey {
        generation,
        page,
        scale_q,
        tile: None,
        dark: false,
    }
}

fn want(keys: impl IntoIterator<Item = PageKey>) -> PdfCmd {
    PdfCmd::Want(keys.into_iter().map(|key| RenderReq { key }).collect())
}

/// Spawns a worker and opens `pdf` as `generation`, returning the page sizes.
fn open(rel: &str, generation: u64) -> (PdfHandle, Receiver<PdfEvent>, Vec<(f64, f64)>) {
    let (tx, rx) = async_channel::unbounded();
    let pdf = lemma_pdf::spawn(tx);
    pdf.send(PdfCmd::Open {
        generation,
        pdf: fixture(rel),
    });
    match next(&rx) {
        PdfEvent::Opened {
            generation: g,
            page_sizes,
        } if g == generation => (pdf, rx, page_sizes),
        other => panic!("expected Opened, got {other:?}"),
    }
}

/// Mean absolute difference per byte between a tile and the same region of a full-page bitmap.
fn tile_diff(full: &Rendered, tile: &Rendered, x0: i32, y0: i32) -> f64 {
    let (fs, ts) = (full.stride as usize, tile.stride as usize);
    let mut sum = 0u64;
    for y in 0..tile.height as usize {
        let f = &full.data[(y0 as usize + y) * fs + x0 as usize * 4..][..tile.width as usize * 4];
        let t = &tile.data[y * ts..][..tile.width as usize * 4];
        sum += f
            .iter()
            .zip(t)
            .map(|(a, b)| u64::from(a.abs_diff(*b)))
            .sum::<u64>();
    }
    sum as f64 / (tile.width as f64 * tile.height as f64 * 4.0)
}

#[test]
fn renders_three_stress_pages_at_2x() {
    let (pdf, rx, sizes) = open("pdf/stress.pdf", 1);
    assert_eq!(sizes.len(), 23);
    assert!(sizes.iter().all(|&s| s == LETTER));
    pdf.send(want([key(1, 0, 16), key(1, 7, 16), key(1, 22, 16)]));
    for page in [0, 7, 22] {
        let r = rendered(&rx);
        assert_eq!(r.key, key(1, page, 16));
        assert_eq!((r.width, r.height), (1224, 1584));
        assert!(r.stride >= r.width * 4);
        assert_eq!(r.data.len(), r.stride as usize * r.height as usize);
        // Opaque white background with some ink on it.
        assert_eq!(&r.data[..4], &[255, 255, 255, 255]);
        assert!(r.data.as_chunks::<4>().0.iter().any(|px| px[0] < 128));
    }
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn dark_requests_come_back_coloured_for_dark_mode() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    let dark = PageKey {
        dark: true,
        ..key(1, 0, 8)
    };
    pdf.send(want([key(1, 0, 8), dark]));
    let light = rendered(&rx);
    let darkened = rendered(&rx);
    assert_eq!(darkened.key, dark);
    assert_eq!(
        (darkened.width, darkened.height),
        (light.width, light.height)
    );
    // Exactly the light bitmap through the dark-mode transform.
    let mut expected = light.data.to_vec();
    lemma_pdf::dark::darken_bgra(&mut expected);
    assert!(expected[..] == darkened.data[..]);
    let paper = (lemma_pdf::dark::PAPER * 255.0).round() as u8;
    assert_eq!(&darkened.data[..4], &[paper, paper, paper, 255]);
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn latest_want_wins_when_sent_back_to_back() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    let old: Vec<_> = (0..20).map(|p| key(1, p, 8)).collect();
    let new: Vec<_> = (20..23).map(|p| key(1, p, 8)).collect();
    pdf.send(want(old));
    pdf.send(want(new.clone()));
    let mut stale = 0;
    let mut got = Vec::new();
    while got.len() < new.len() {
        let r = rendered(&rx);
        if r.key.page < 20 {
            stale += 1;
            assert!(got.is_empty(), "an old page after a new one: {r:?}");
        } else {
            got.push(r.key);
        }
    }
    assert!(
        stale <= 1,
        "{stale} pages of the replaced Want were rendered"
    );
    assert_eq!(got, new);
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn latest_want_wins_mid_queue() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    pdf.send(want((0..20).map(|p| key(1, p, 16))));
    assert_eq!(rendered(&rx).key, key(1, 0, 16));
    // The worker is now rendering (or about to render) page 1; at most that page may still arrive.
    pdf.send(want([key(1, 21, 8)]));
    let mut stale = 0;
    loop {
        let r = rendered(&rx);
        if r.key == key(1, 21, 8) {
            break;
        }
        assert!(r.key.page < 20, "unexpected {r:?}");
        stale += 1;
    }
    assert!(
        stale <= 1,
        "{stale} pages of the replaced Want were rendered"
    );
    assert!(matches!(
        try_next(&rx, Duration::from_millis(200)),
        Err(TryRecvError::Empty)
    ));
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn stale_generations_are_dropped() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    pdf.send(PdfCmd::Open {
        generation: 2,
        pdf: fixture("pdf/test.pdf"),
    });
    pdf.send(want([
        key(1, 0, 8),
        key(2, 0, 8),
        key(1, 1, 8),
        key(2, 1, 8),
        key(2, 2, 8),
    ]));
    match next(&rx) {
        PdfEvent::Opened {
            generation: 2,
            page_sizes,
        } => assert_eq!(page_sizes.len(), 3),
        other => panic!("expected Opened, got {other:?}"),
    }
    for page in 0..3 {
        assert_eq!(rendered(&rx).key, key(2, page, 8));
    }
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn reopening_drops_the_old_generations_queue() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    pdf.send(want((0..23).map(|p| key(1, p, 16))));
    pdf.send(PdfCmd::Open {
        generation: 2,
        pdf: fixture("pdf/stress.pdf"),
    });
    pdf.send(want([key(2, 5, 8)]));
    let mut old = 0;
    loop {
        match next(&rx) {
            PdfEvent::Rendered(r) if r.key.generation == 1 => old += 1,
            PdfEvent::Opened { generation: 2, .. } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(old <= 1, "{old} pages of generation 1 were rendered");
    assert_eq!(rendered(&rx).key, key(2, 5, 8));
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn failed_open_keeps_the_current_document() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    pdf.send(PdfCmd::Open {
        generation: 2,
        pdf: fixture("pdf").join("missing.pdf"),
    });
    match next(&rx) {
        PdfEvent::OpenFailed {
            generation: 2,
            error,
        } => assert!(error.contains("missing.pdf")),
        other => panic!("expected OpenFailed, got {other:?}"),
    }
    pdf.send(PdfCmd::Open {
        generation: 3,
        pdf: fixture("pdf/test.tex"),
    });
    assert!(matches!(
        next(&rx),
        PdfEvent::OpenFailed { generation: 3, .. }
    ));
    pdf.send(want([key(3, 0, 8), key(2, 0, 8), key(1, 3, 8)]));
    assert_eq!(rendered(&rx).key, key(1, 3, 8));
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn large_pages_are_rendered_as_tiles() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    // 3x letter is 17.4 MB, over the 16 MiB threshold: the whole-page request becomes 5
    // full-width bands.
    let (cols, rows) = tile_grid(LETTER, 24).expect("3x letter is tiled");
    assert_eq!((cols, rows), (1, 5));
    pdf.send(want([key(1, 2, 24)]));
    let (w, h) = page_pixels(LETTER, 24);
    let mut area = 0i64;
    for row in 0..rows {
        for col in 0..cols {
            let r = rendered(&rx);
            assert_eq!(r.key.tile, Some((col, row)));
            let (x, y, tw, th) = tile_rect(LETTER, 24, (col, row));
            assert_eq!((r.width, r.height), (tw, th));
            assert_eq!(tw, w, "a full-width band");
            assert!(x + tw <= w && y + th <= h);
            assert_eq!(r.data.len(), r.stride as usize * r.height as usize);
            area += i64::from(r.width) * i64::from(r.height);
        }
    }
    assert_eq!(area, i64::from(w) * i64::from(h));

    // An explicit tile (the lower of 2x letter's two bands) matches the same region of the
    // full-page bitmap.
    let tile = PageKey {
        tile: Some((0, 1)),
        ..key(1, 4, 16)
    };
    pdf.send(want([key(1, 4, 16), tile]));
    let full = rendered(&rx);
    let part = rendered(&rx);
    assert_eq!(part.key, tile);
    assert_eq!((part.width, part.height), (1224, 792));
    let diff = tile_diff(&full, &part, 0, 792);
    assert!(
        diff < 0.5,
        "tile differs from the full page by {diff}/255 per byte"
    );
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn links_come_from_the_link_mapping() {
    let (pdf, rx, sizes) = open("proj/main.pdf", 4);
    let mut internal = 0;
    for page in 0..sizes.len() as u32 {
        pdf.send(PdfCmd::Links { page });
        let PdfEvent::Links {
            generation,
            page: p,
            links,
        } = next(&rx)
        else {
            panic!("expected Links");
        };
        assert_eq!((generation, p), (4, page));
        let (pw, ph) = sizes[page as usize];
        for link in &links {
            let (x, y, w, h) = link.area;
            assert!(w > 0.0 && h > 0.0, "{link:?}");
            assert!(
                x >= 0.0 && y >= 0.0 && x + w <= pw + 0.5 && y + h <= ph + 0.5,
                "{link:?}"
            );
            if let LinkDest::Page { page, y_bp } = link.dest {
                internal += 1;
                assert!((page as usize) < sizes.len(), "{link:?}");
                if let Some(y) = y_bp {
                    assert!((0.0..=sizes[page as usize].1).contains(&y), "{link:?}");
                }
            }
        }
    }
    assert!(internal > 0, "hyperref cross-references resolve to pages");
    pdf.send(PdfCmd::Links { page: 99 });
    assert!(matches!(next(&rx), PdfEvent::Links { page: 99, links, .. } if links.is_empty()));
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn forward_and_inverse_go_through_synctex() {
    assert!(
        lemma_synctex::Scanner::open(&fixture("proj/main.pdf")).is_some(),
        "the proj fixture has SyncTeX data"
    );
    let (pdf, rx, _) = open("proj/main.pdf", 7);
    pdf.send(PdfCmd::Forward {
        file: fixture("proj/main.tex"),
        line: 66,
    });
    match next(&rx) {
        PdfEvent::Forward { generation, boxes } => {
            assert_eq!(generation, 7);
            let b = boxes.first().expect("line 66 has a box");
            assert_eq!(b.page, 2);
            assert!(
                (b.h - 126.67).abs() < 0.1 && (b.v_top - 415.05).abs() < 0.1,
                "{b:?}"
            );
        }
        other => panic!("expected Forward, got {other:?}"),
    }
    // Page index 1 is SyncTeX's page 2.
    pdf.send(PdfCmd::Inverse {
        page: 1,
        x_bp: 200.0,
        y_bp: 420.0,
    });
    match next(&rx) {
        PdfEvent::Inverse {
            target: Some(target),
        } => {
            assert!(target.file.ends_with("main.tex"), "{target:?}");
            assert_eq!(target.line, 66);
        }
        other => panic!("expected an Inverse target, got {other:?}"),
    }
    pdf.send(PdfCmd::Forward {
        file: fixture("proj/chapters/yoneda.tex"),
        line: 26,
    });
    assert!(
        matches!(next(&rx), PdfEvent::Forward { boxes, .. } if boxes.first().is_some_and(|b| b.page == 2))
    );
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn sync_without_synctex_data_answers_empty() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 2);
    // stress.pdf has no .synctex.gz, so Scanner::open is never reached.
    pdf.send(PdfCmd::Forward {
        file: fixture("pdf/test.tex"),
        line: 1,
    });
    assert!(matches!(next(&rx), PdfEvent::Forward { generation: 2, boxes } if boxes.is_empty()));
    pdf.send(PdfCmd::Inverse {
        page: 0,
        x_bp: 10.0,
        y_bp: 10.0,
    });
    assert!(matches!(next(&rx), PdfEvent::Inverse { target: None }));
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn queries_without_a_document_are_answered() {
    let (tx, rx) = async_channel::unbounded();
    let pdf = lemma_pdf::spawn(tx);
    pdf.send(want([key(0, 0, 8)]));
    pdf.send(PdfCmd::Forward {
        file: fixture("proj/main.tex"),
        line: 66,
    });
    assert!(matches!(next(&rx), PdfEvent::Forward { generation: 0, boxes } if boxes.is_empty()));
    pdf.send(PdfCmd::Links { page: 0 });
    assert!(matches!(next(&rx), PdfEvent::Links { generation: 0, links, .. } if links.is_empty()));
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn shutdown_and_dropped_handles_stop_the_thread() {
    let (pdf, rx, _) = open("pdf/test.pdf", 1);
    pdf.send(want((0..3).map(|p| key(1, p, 8))));
    pdf.send(PdfCmd::Shutdown);
    // The thread drops its event sender when it exits, which closes the channel.
    loop {
        match try_next(&rx, TIMEOUT) {
            Ok(PdfEvent::Rendered(_)) => {}
            Err(TryRecvError::Closed) => break,
            other => panic!("expected the channel to close, got {other:?}"),
        }
    }
    pdf.send(PdfCmd::Links { page: 0 });

    // Dropping the last handle stops the thread and waits for it (it has freed its document and
    // dropped its event sender by the time `drop` returns); a clone keeps it running.
    let (pdf, rx, _) = open("pdf/test.pdf", 1);
    let clone = pdf.clone();
    drop(pdf);
    clone.send(PdfCmd::Links { page: 0 });
    assert!(matches!(next(&rx), PdfEvent::Links { generation: 1, .. }));
    drop(clone);
    assert!(rx.is_closed(), "the thread has ended");
    assert!(matches!(try_next(&rx, TIMEOUT), Err(TryRecvError::Closed)));
}

#[test]
fn shutdown_and_join_waits_for_the_thread() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    let clone = pdf.clone();
    pdf.send(want((0..23).map(|p| key(1, p, 16))));
    assert!(pdf.shutdown_and_join(TIMEOUT));
    // The thread has ended: its event sender is gone (at most the page being rendered when
    // Shutdown arrived was still sent), and further commands from any handle are ignored.
    assert!(rx.is_closed());
    let mut rendered = 0;
    while let Ok(event) = rx.try_recv() {
        assert!(matches!(event, PdfEvent::Rendered(_)), "{event:?}");
        rendered += 1;
    }
    assert!(rendered < 23, "pending requests were dropped");
    clone.send(PdfCmd::Links { page: 0 });
    assert!(clone.shutdown_and_join(Duration::ZERO), "already joined");
}

#[test]
#[ignore = "perf: run with cargo test --release -- --ignored perf"]
fn perf_render() {
    let (pdf, rx, sizes) = open("pdf/stress.pdf", 1);
    let n = sizes.len() as u32;
    // Warm up fonts and caches.
    pdf.send(want([key(1, 0, 16)]));
    rendered(&rx);
    let t = Instant::now();
    pdf.send(want((0..n).map(|p| key(1, p, 16))));
    for _ in 0..n {
        rendered(&rx);
    }
    let per_page = t.elapsed().as_secs_f64() * 1e3 / f64::from(n);
    eprintln!("perf_render: {per_page:.2} ms per stress page at 2x");
    assert!(per_page < 15.0, "{per_page:.2} ms per page");
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

/// Renders the regions `rects` (device pixels) of `page` at `scale` as the PDF thread renders a
/// tile: each on a white surface of its own, through a translated and scaled context.
fn render_regions(page: &poppler::Page, scale: f64, rects: &[(i32, i32, i32, i32)]) {
    for &(x, y, w, h) in rects {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint().unwrap();
        cr.translate(-f64::from(x), -f64::from(y));
        cr.scale(scale, scale);
        page.render(&cr);
        cr.status().unwrap();
    }
}

/// The 512² squares that tiled a `w × h` px page before, under the region `(x, y, width, height)`
/// of it.
fn squares_under(
    w: i32,
    h: i32,
    (x, y, width, height): (i32, i32, i32, i32),
) -> Vec<(i32, i32, i32, i32)> {
    let (x0, y0) = (x / 512 * 512, y / 512 * 512);
    (y0..(y + height).min(h))
        .step_by(512)
        .flat_map(|ty| {
            (x0..(x + width).min(w))
                .step_by(512)
                .map(move |tx| (tx, ty, 512.min(w - tx), 512.min(h - ty)))
        })
        .collect()
}

/// A page too large for one bitmap costs a poppler pass over its whole content per tile, so its
/// full-width bands must cost well under the 512² tiles that tiled it before. The stress pages at
/// scale_q 26 (1989 × 2574 px, a fit-width page on a HiDPI pane about 1000 px wide), measured on
/// a loaded machine: 5 bands 23–29 ms, 24 tiles 77–93 ms, the whole page 10–12 ms. Zoomed in far
/// (scale_q 96, 7344 × 9504 px), the tiles of about 1024² under a 1800 × 2000 px view must cost
/// well under the 512² ones too: 9 tiles (33 MiB) 31–34 ms, 25 tiles (25 MiB) 77–78 ms.
#[test]
#[ignore = "perf: run with cargo test --release -- --ignored perf"]
fn perf_tiled_page() {
    const Q: u16 = 26;
    let (pdf, rx, sizes) = open("pdf/stress.pdf", 1);
    let n = sizes.len() as u32;
    let (cols, rows) = tile_grid(LETTER, Q).expect("tiled at scale_q 26");
    let bands = usize::from(cols) * usize::from(rows);
    // Warm up fonts and caches.
    pdf.send(want([key(1, 0, Q)]));
    for _ in 0..bands {
        rendered(&rx);
    }
    // Through the PDF thread, which answers a whole-page request with the page's bands.
    let t = Instant::now();
    pdf.send(want((0..n).map(|p| key(1, p, Q))));
    for _ in 0..n as usize * bands {
        rendered(&rx);
    }
    let banded = t.elapsed().as_secs_f64() * 1e3 / f64::from(n);
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");

    // The same pages with poppler as the PDF thread calls it: whole, and in 512² tiles.
    let bytes = glib::Bytes::from_owned(std::fs::read(fixture("pdf/stress.pdf")).unwrap());
    let doc = poppler::Document::from_bytes(&bytes, None).unwrap();
    let time = |q: u16, rects: &[(i32, i32, i32, i32)]| {
        let t = Instant::now();
        for p in 0..n as i32 {
            render_regions(&doc.page(p).unwrap(), lemma_pdf::scale_of(q), rects);
        }
        t.elapsed().as_secs_f64() * 1e3 / f64::from(n)
    };
    let (w, h) = page_pixels(LETTER, Q);
    let squares = squares_under(w, h, (0, 0, w, h));
    render_regions(
        &doc.page(0).unwrap(),
        lemma_pdf::scale_of(Q),
        &[(0, 0, w, h)],
    );
    let whole = time(Q, &[(0, 0, w, h)]);
    let tiled = time(Q, &squares);
    eprintln!(
        "perf_tiled_page: a {w} × {h} px stress page in {bands} bands {banded:.1} ms, in {} tiles \
         of 512² {tiled:.1} ms, whole {whole:.1} ms",
        squares.len()
    );
    assert_eq!((bands, squares.len()), (5, 24));
    assert!(
        banded * 2.0 < tiled,
        "{bands} bands {banded:.1} ms, {} tiles {tiled:.1} ms",
        squares.len()
    );

    // Zoomed in far: a 900 × 1000 view at device scale 2 in the middle of the page, off the
    // edges of tiles and squares.
    const ZOOMED_Q: u16 = 96;
    let (w, h) = page_pixels(LETTER, ZOOMED_Q);
    let view = ((w - 1800) / 2 + 37, (h - 2000) / 2 + 41, 1800, 2000);
    let tiles: Vec<(i32, i32, i32, i32)> = tiles_covering(LETTER, ZOOMED_Q, view)
        .expect("tiled at scale_q 96")
        .into_iter()
        .map(|tile| tile_rect(LETTER, ZOOMED_Q, tile))
        .collect();
    let squares = squares_under(w, h, view);
    let mib = |rects: &[(i32, i32, i32, i32)]| {
        rects
            .iter()
            .map(|r| f64::from(r.2) * f64::from(r.3) * 4.0)
            .sum::<f64>()
            / f64::from(1 << 20)
    };
    let zoomed = time(ZOOMED_Q, &tiles);
    let zoomed_squares = time(ZOOMED_Q, &squares);
    eprintln!(
        "perf_tiled_page: a 1800 × 2000 px view of a {w} × {h} px stress page in {} tiles \
         ({:.0} MiB) {zoomed:.1} ms, in {} tiles of 512² ({:.0} MiB) {zoomed_squares:.1} ms",
        tiles.len(),
        mib(&tiles),
        squares.len(),
        mib(&squares)
    );
    assert_eq!((tiles.len(), squares.len()), (9, 25));
    assert!(
        zoomed * 1.5 < zoomed_squares,
        "{} tiles {zoomed:.1} ms, {} tiles of 512² {zoomed_squares:.1} ms",
        tiles.len(),
        squares.len()
    );
}
