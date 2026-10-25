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

/// `lemma_pdf::dark::darken_bgra` before its fast paths for white and grey pixels (2026-10-25),
/// which must not change a bit of what it gives.
fn darken_bgra_reference(data: &mut [u8]) {
    use lemma_pdf::dark::{HUE_180, INK, PAPER, darken_rgb};
    let contrast = INK - PAPER;
    let mut lut = [[[0i32; 256]; 3]; 3];
    for (c, row) in HUE_180.iter().enumerate() {
        for (j, h) in row.iter().enumerate() {
            for (v, entry) in lut[c][j].iter_mut().enumerate() {
                *entry = (contrast * h * v as f32 * 256.0).round() as i32;
            }
        }
    }
    let base = (INK * 255.0 * 256.0).round() as i32;
    let level = |c: usize, r: u8, g: u8, b: u8| -> u8 {
        let sum = lut[c][0][usize::from(r)] + lut[c][1][usize::from(g)] + lut[c][2][usize::from(b)];
        ((base - sum + 128) >> 8).clamp(0, 255) as u8
    };
    for pixel in data.as_chunks_mut::<4>().0 {
        let (b, g, r, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        match a {
            255 => {
                pixel[2] = level(0, r, g, b);
                pixel[1] = level(1, r, g, b);
                pixel[0] = level(2, r, g, b);
            }
            0 => pixel[..3].fill(0),
            _ => {
                let alpha = f32::from(a) / 255.0;
                let straight = |v: u8| (f32::from(v) / 255.0 / alpha).min(1.0);
                let out = darken_rgb([straight(r), straight(g), straight(b)]);
                let premultiplied = |v: f32| (v * alpha * 255.0).round() as u8;
                pixel[2] = premultiplied(out[0]);
                pixel[1] = premultiplied(out[1]);
                pixel[0] = premultiplied(out[2]);
            }
        }
    }
}

/// The dark-mode colouring of `data` by both, which must be equal byte for byte.
fn assert_darkened_as_before(data: &[u8], what: &str) {
    let mut expected = data.to_vec();
    darken_bgra_reference(&mut expected);
    let mut darkened = data.to_vec();
    lemma_pdf::dark::darken_bgra(&mut darkened);
    if let Some(i) = expected.iter().zip(&darkened).position(|(a, b)| a != b) {
        let pixel = i / 4 * 4;
        panic!(
            "{what}: pixel {} {:?} became {:?}, not {:?}",
            i / 4,
            &data[pixel..pixel + 4],
            &darkened[pixel..pixel + 4],
            &expected[pixel..pixel + 4]
        );
    }
}

#[test]
fn dark_pages_come_out_as_before() {
    let (pdf, rx, _) = open("pdf/stress.pdf", 1);
    // Pages of text, a table and coloured links, at scale 1 and fit width on a HiDPI screen; and
    // their dark bitmaps coloured again, whose pixels are other greys and colours.
    for (page, scale_q) in [(0, 8), (5, 8), (11, 21), (19, 21)] {
        let dark = PageKey {
            dark: true,
            ..key(1, page, scale_q)
        };
        pdf.send(want([key(1, page, scale_q), dark]));
        let light = rendered(&rx);
        let darkened = rendered(&rx);
        let mut expected = light.data.to_vec();
        darken_bgra_reference(&mut expected);
        assert!(
            expected[..] == darkened.data[..],
            "page {page} at scale_q {scale_q}"
        );
        assert_darkened_as_before(&light.data, &format!("page {page} light"));
        assert_darkened_as_before(&darkened.data, &format!("page {page} dark"));
    }
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
    // Every opaque colour, and translucent and transparent pixels.
    let mut every = Vec::with_capacity(4 << 24);
    for r in 0..=255u8 {
        for g in 0..=255u8 {
            for b in 0..=255u8 {
                every.extend_from_slice(&[b, g, r, 255]);
            }
        }
    }
    for a in 0..=254u8 {
        for v in (0..=a).step_by(5) {
            every.extend_from_slice(&[v, v, v, a, v, a / 2, 0, a]);
        }
    }
    assert_darkened_as_before(&every, "every colour");
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

/// A fresh directory for one test (canonical, so recorded SyncTeX paths match).
fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-pdf-worker-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    lemma_core::canonicalize(&dir).unwrap()
}

/// Writes `main.pdf` in `dir`: pages of `sizes` (bp, unturned) with `/Rotate rotate`, the first
/// with a black 300 × 20 bp rectangle at x 72..372, y 172..192 of its unturned page (top-left
/// origin); and `main.synctex`, in which line 5 of `main.tex` is an hbox over that rectangle.
fn write_pdf(dir: &Path, sizes: &[(f64, f64)], rotate: u32) -> PathBuf {
    use std::fmt::Write as _;
    let content = format!("0 g 72 {} 300 20 re f\n", sizes[0].1 - 192.0);
    let kids: Vec<String> = (0..sizes.len()).map(|p| format!("{} 0 R", p + 4)).collect();
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            sizes.len()
        ),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
    ];
    for (i, (w, h)) in sizes.iter().enumerate() {
        let contents = if i == 0 { " /Contents 3 0 R" } else { "" };
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Rotate {rotate} \
             /Resources << >>{contents} >>"
        ));
    }
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
    write!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
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
    // Height 18 above the baseline at 190, depth 2 below it.
    writeln!(
        s,
        "(1,5:{},{}:{},{},{}",
        sp(72.0),
        sp(190.0),
        sp(300.0),
        sp(18.0),
        sp(2.0)
    )
    .unwrap();
    writeln!(s, "g1,5:{},{}\n)", sp(72.0), sp(190.0)).unwrap();
    s.push_str("]\n}1\nPostamble:\nCount:1\nPost scriptum:\n");
    std::fs::write(dir.join("main.synctex"), s).unwrap();
    path
}

/// Spawns a worker and opens `pdf` as generation 1, returning its page sizes.
fn open_path(pdf: &Path) -> (PdfHandle, Receiver<PdfEvent>, Vec<(f64, f64)>) {
    let (tx, rx) = async_channel::unbounded();
    let handle = lemma_pdf::spawn(tx);
    handle.send(PdfCmd::Open {
        generation: 1,
        pdf: pdf.to_path_buf(),
    });
    match next(&rx) {
        PdfEvent::Opened {
            generation: 1,
            page_sizes,
        } => (handle, rx, page_sizes),
        other => panic!("expected Opened, got {other:?}"),
    }
}

#[test]
fn a_reload_shows_at_once_and_reports_the_sizes_it_reads() {
    let dir = temp_dir("reload");
    let mut sizes = vec![LETTER; 40];
    let pdf = write_pdf(&dir, &sizes, 0);
    let (handle, rx, opened) = open_path(&pdf);
    assert_eq!(opened, sizes);
    // A rebuild with as many pages, one of them larger: it shows with the sizes of the
    // generation before, and the pages read afterwards correct them.
    sizes[30] = (500.0, 700.0);
    write_pdf(&dir, &sizes, 0);
    handle.send(PdfCmd::Reload {
        generation: 2,
        pdf: pdf.clone(),
    });
    match next(&rx) {
        PdfEvent::Opened {
            generation: 2,
            page_sizes,
        } => assert_eq!(page_sizes, [LETTER; 40]),
        other => panic!("expected Opened, got {other:?}"),
    }
    match next(&rx) {
        PdfEvent::PageSizes {
            generation: 2,
            page_sizes,
        } => assert_eq!(page_sizes, sizes),
        other => panic!("expected PageSizes, got {other:?}"),
    }
    handle.send(want([key(2, 30, 8)]));
    let page = rendered(&rx);
    assert_eq!((page.width, page.height), (500, 700));
    // Another file: every page is read before it shows.
    let other = temp_dir("reload-other");
    let other_pdf = write_pdf(&other, &[(595.0, 842.0); 40], 0);
    handle.send(PdfCmd::Reload {
        generation: 3,
        pdf: other_pdf,
    });
    match next(&rx) {
        PdfEvent::Opened {
            generation: 3,
            page_sizes,
        } => assert_eq!(page_sizes, [(595.0, 842.0); 40]),
        other => panic!("expected Opened, got {other:?}"),
    }
    handle.send(want([key(3, 0, 8)]));
    assert_eq!(rendered(&rx).key, key(3, 0, 8), "no PageSizes before it");
    assert!(handle.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn a_reload_maps_synctex_on_a_page_that_turned() {
    let dir = temp_dir("reload-turned");
    let pdf = write_pdf(&dir, &[LETTER], 0);
    let (handle, rx, opened) = open_path(&pdf);
    assert_eq!(opened, [LETTER]);
    // The page is turned by /Rotate 90 now (shown 792 × 612); its rectangle shows at x 600..620,
    // y 72..372.
    write_pdf(&dir, &[LETTER], 90);
    handle.send(PdfCmd::Reload {
        generation: 2,
        pdf: pdf.clone(),
    });
    assert!(matches!(
        next(&rx),
        PdfEvent::Opened { generation: 2, page_sizes } if page_sizes == [LETTER]
    ));
    handle.send(PdfCmd::Forward {
        file: dir.join("main.tex"),
        line: 5,
    });
    handle.send(PdfCmd::Inverse {
        page: 0,
        x_bp: 610.0,
        y_bp: 222.0,
    });
    // The page's new size comes first, whether the pass read it or the query.
    match next(&rx) {
        PdfEvent::PageSizes {
            generation: 2,
            page_sizes,
        } => assert_eq!(page_sizes, [(792.0, 612.0)]),
        other => panic!("expected PageSizes, got {other:?}"),
    }
    match next(&rx) {
        PdfEvent::Forward {
            generation: 2,
            boxes,
        } => {
            let b = boxes.first().expect("line 5 has a box");
            let near = |a: f32, b: f32| (a - b).abs() < 0.5;
            assert!(
                near(b.h, 600.0) && near(b.v_top, 72.0) && near(b.width, 20.0),
                "{b:?} on the turned page"
            );
            assert!(near(b.height, 300.0), "{b:?} on the turned page");
        }
        other => panic!("expected Forward, got {other:?}"),
    }
    match next(&rx) {
        PdfEvent::Inverse {
            target: Some(target),
        } => assert_eq!(target.line, 5),
        other => panic!("expected an Inverse target, got {other:?}"),
    }
    assert!(handle.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

#[test]
fn trim_keeps_sync_working() {
    let (pdf, rx, _) = open("proj/main.pdf", 1);
    let forward = || PdfCmd::Forward {
        file: fixture("proj/main.tex"),
        line: 66,
    };
    pdf.send(forward());
    assert!(matches!(next(&rx), PdfEvent::Forward { boxes, .. } if !boxes.is_empty()));
    // The scanner goes; the next query parses the file again.
    pdf.send(PdfCmd::Trim);
    pdf.send(forward());
    assert!(matches!(next(&rx), PdfEvent::Forward { boxes, .. } if !boxes.is_empty()));
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

/// `target/fixtures/pdf50.pdf`, when `scripts/build-fixtures.sh` has built it.
fn pdf50() -> Option<PathBuf> {
    let pdf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fixtures/pdf50.pdf");
    pdf.is_file()
        .then(|| lemma_core::canonicalize(&pdf).expect("canonical fixture path"))
}

/// A PDF of your own for the timing tests (`LEMMA_PERF_PDF`: a long build, with its SyncTeX
/// file for `perf_first_sync_after_a_rebuild`).
fn perf_pdf() -> Option<PathBuf> {
    std::env::var_os("LEMMA_PERF_PDF").map(PathBuf::from)
}

/// The median and the largest of `times`.
fn median_max(times: &mut [f64]) -> (f64, f64) {
    times.sort_by(f64::total_cmp);
    (times[times.len() / 2], times[times.len() - 1])
}

fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// The one-minute load average, for the printed timings.
fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|l| l.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

/// `page` rendered whole at `scale_q` as the PDF thread renders it, light.
fn render_light(page: &poppler::Page, scale_q: u16) -> Vec<u8> {
    let (w, h) = page_pixels(page.size(), scale_q);
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
    {
        let cr = cairo::Context::new(&surface).unwrap();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint().unwrap();
        let s = lemma_pdf::scale_of(scale_q);
        cr.scale(s, s);
        page.render(&cr);
        cr.status().unwrap();
    }
    surface.data().unwrap().to_vec()
}

/// Dark pages cost little more than light ones. Colouring a page takes a fraction of what it took
/// without the fast paths for white and grey pixels, at scale_q 10 (fit width at scale 1) and 21
/// (fit width on a HiDPI screen), on the stress pages and pdf50's (when built); and through the
/// PDF thread a dark stress page at scale_q 21 costs at most half again a light one. Measured at
/// load 4–9: 2.5–2.7 → 0.4 ms and 11.3–11.6 → 2.4 ms a page; through the thread 18.6 ms a dark
/// page before, against 7.9 light.
#[test]
#[ignore = "perf: run with cargo test --release -- --ignored perf"]
fn perf_dark_pages() {
    let mut pdfs = vec![fixture("pdf/stress.pdf")];
    pdfs.extend(pdf50());
    for pdf in &pdfs {
        let bytes = glib::Bytes::from_owned(std::fs::read(pdf).unwrap());
        let doc = poppler::Document::from_bytes(&bytes, None).unwrap();
        for scale_q in [10u16, 21] {
            let (mut before, mut after) = (Vec::new(), Vec::new());
            for p in 0..4 {
                let light = render_light(&doc.page(p).unwrap(), scale_q);
                for _ in 0..5 {
                    let mut reference = light.clone();
                    let t = Instant::now();
                    darken_bgra_reference(&mut reference);
                    before.push(ms_since(t));
                    let mut darkened = light.clone();
                    let t = Instant::now();
                    lemma_pdf::dark::darken_bgra(&mut darkened);
                    after.push(ms_since(t));
                    assert!(reference == darkened, "page {p} at scale_q {scale_q}");
                }
            }
            let ((before, before_max), (after, after_max)) =
                (median_max(&mut before), median_max(&mut after));
            eprintln!(
                "perf_dark_pages: {} at scale_q {scale_q}: {before:.2} / {before_max:.2} ms a page \
                 without the fast paths, {after:.2} / {after_max:.2} ms with them (median / max, \
                 load {})",
                pdf.file_name().unwrap().to_string_lossy(),
                load_average()
            );
            assert!(
                after * 2.5 < before,
                "{after:.2} ms with the fast paths, {before:.2} ms without"
            );
        }
    }

    // Through the PDF thread, light and dark in turn.
    const Q: u16 = 21;
    let (pdf, rx, sizes) = open("pdf/stress.pdf", 1);
    let n = sizes.len() as u32;
    let per_page = |dark: bool| {
        let page = |p| PageKey {
            dark,
            ..key(1, p, Q)
        };
        pdf.send(want([page(0)]));
        rendered(&rx);
        let t = Instant::now();
        pdf.send(want((0..n).map(page)));
        for _ in 0..n {
            rendered(&rx);
        }
        ms_since(t) / f64::from(n)
    };
    let (mut light, mut dark) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        light.push(per_page(false));
        dark.push(per_page(true));
    }
    let ((light, light_max), (dark, dark_max)) = (median_max(&mut light), median_max(&mut dark));
    eprintln!(
        "perf_dark_pages: a stress page at scale_q {Q} through the PDF thread: light {light:.1} / \
         {light_max:.1} ms, dark {dark:.1} / {dark_max:.1} ms (median / max, load {})",
        load_average()
    );
    assert!(dark < light * 1.5, "dark {dark:.1} ms, light {light:.1} ms");
    assert!(pdf.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

/// Gives the SyncTeX file next to `pdf` another modification time, as a rebuild does.
fn touch_synctex(pdf: &Path, n: u64) {
    std::fs::File::options()
        .write(true)
        .open(pdf.with_extension("synctex.gz"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n))
        .unwrap();
}

/// Waits for the `Opened` of `generation`, dropping other events.
fn opened(rx: &Receiver<PdfEvent>, generation: u64) {
    loop {
        if let PdfEvent::Opened { generation: g, .. } = next(rx)
            && g == generation
        {
            return;
        }
    }
}

/// Asks for the first three pages of `generation` at scale_q 10 (a view at fit width) and
/// returns how long the first one took to come, once all three have.
fn first_of_three_pages(pdf: &PdfHandle, rx: &Receiver<PdfEvent>, generation: u64) -> f64 {
    let t = Instant::now();
    pdf.send(want((0..3).map(|p| key(generation, p, 10))));
    let mut first = None;
    let mut pages = 0;
    while pages < 3 {
        match next(rx) {
            PdfEvent::Rendered(r) => {
                assert_eq!(r.key.generation, generation);
                first.get_or_insert_with(|| ms_since(t));
                pages += 1;
            }
            PdfEvent::PageSizes { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    first.unwrap_or_default()
}

/// The first sync after a rebuild holds no page back: with an Inverse (a Ctrl+click) sent just
/// before the Want of the visible pages, the first page comes about as soon as without it, while
/// the SyncTeX file is parsed on a helper thread; and once a rebuild's file has been parsed ahead
/// (SyncTeX was used before), its first Forward is answered within §7's 5 ms. On pdf50 (built by
/// `scripts/build-fixtures.sh`), or `LEMMA_PERF_PDF`. Measured on a 500-page build at load 4–8,
/// where the parse takes 125–135 ms: the first page came after 136–147 ms with the Inverse
/// (9–10 ms without it), and the first Forward after a rebuild took 126–131 ms.
#[test]
#[ignore = "perf: run with cargo test --release -- --ignored perf"]
fn perf_first_sync_after_a_rebuild() {
    let Some(src) = perf_pdf().or_else(pdf50) else {
        eprintln!("SKIP: run scripts/build-fixtures.sh (or set LEMMA_PERF_PDF)");
        return;
    };
    let dir = temp_dir("perf-sync");
    let pdf = dir.join(src.file_name().unwrap());
    std::fs::copy(&src, &pdf).unwrap();
    std::fs::copy(
        src.with_extension("synctex.gz"),
        pdf.with_extension("synctex.gz"),
    )
    .unwrap();
    // The SyncTeX file names the sources where they were built.
    let source = src.with_extension("tex");
    let (handle, rx, _) = open_path(&pdf);
    let forward = || PdfCmd::Forward {
        file: source.clone(),
        line: 20,
    };
    handle.send(forward());
    assert!(matches!(next(&rx), PdfEvent::Forward { .. }));
    let reload = |generation| {
        handle.send(PdfCmd::Reload {
            generation,
            pdf: pdf.clone(),
        });
        opened(&rx, generation);
    };
    let (mut held, mut parse, mut alone, mut ahead) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut generation = 1;
    for run in 0..5 {
        // A rebuild, then a Ctrl+click on it and its pages.
        touch_synctex(&pdf, run * 2);
        generation += 1;
        reload(generation);
        let t = Instant::now();
        handle.send(PdfCmd::Inverse {
            page: 0,
            x_bp: 200.0,
            y_bp: 300.0,
        });
        handle.send(want((0..3).map(|p| key(generation, p, 10))));
        let (mut first, mut inverse, mut pages) = (None, None, 0);
        while pages < 3 || inverse.is_none() {
            match next(&rx) {
                PdfEvent::Rendered(_) => {
                    pages += 1;
                    first.get_or_insert_with(|| ms_since(t));
                }
                PdfEvent::Inverse { .. } => inverse = Some(ms_since(t)),
                PdfEvent::PageSizes { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        held.extend(first);
        parse.extend(inverse);
        // The same pages without a sync (the file is parsed already).
        generation += 1;
        reload(generation);
        alone.push(first_of_three_pages(&handle, &rx, generation));
        // A rebuild parsed ahead, then a Forward once the parse is over.
        touch_synctex(&pdf, run * 2 + 1);
        generation += 1;
        reload(generation);
        let longest = parse.iter().copied().fold(0.0, f64::max);
        std::thread::sleep(Duration::from_secs_f64(longest * 3.0 / 1e3 + 0.1));
        let t = Instant::now();
        handle.send(forward());
        assert!(matches!(next(&rx), PdfEvent::Forward { .. }));
        ahead.push(ms_since(t));
    }
    let [held, parse, alone, ahead] =
        [&mut held, &mut parse, &mut alone, &mut ahead].map(|times| median_max(times));
    eprintln!(
        "perf_first_sync_after_a_rebuild: {}: the first page {:.1} / {:.1} ms with a Ctrl+click \
         answered after {:.1} / {:.1} ms, {:.1} / {:.1} ms without; the first Forward after a \
         rebuild parsed ahead {:.2} / {:.2} ms (median / max, load {})",
        src.display(),
        held.0,
        held.1,
        parse.0,
        parse.1,
        alone.0,
        alone.1,
        ahead.0,
        ahead.1,
        load_average()
    );
    assert!(
        held.0 <= alone.0 * 1.5 + 5.0,
        "the first page {:.1} ms with a sync, {:.1} ms without",
        held.0,
        alone.0
    );
    assert!(ahead.0 <= 5.0, "the first Forward took {:.2} ms", ahead.0);
    assert!(handle.shutdown_and_join(TIMEOUT), "the pdf thread stops");
}

/// Writes `pages` A4 pages of a line of text and eight web links each, as a stand-in for a long
/// build (pdfTeX's pages, in object streams, take about five times as long to read).
fn write_long_pdf(path: &Path, pages: usize) {
    use std::fmt::Write as _;
    let mut objects = vec![
        String::new(),
        String::new(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut kids = Vec::with_capacity(pages);
    for p in 0..pages {
        let mut annots = Vec::new();
        for l in 0..8 {
            objects.push(format!(
                "<< /Type /Annot /Subtype /Link /Rect [{} 700 {} 712] /Border [0 0 0] \
                 /A << /S /URI /URI (https://example.org/{p}/{l}) >> >>",
                72 + l * 50,
                112 + l * 50
            ));
            annots.push(format!("{} 0 R", objects.len()));
        }
        let content = format!(
            "BT /F1 11 Tf 72 740 Td (Page {} of a long document) Tj ET\n",
            p + 1
        );
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ));
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595.276 841.89] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R /Annots [{}] >>",
            objects.len(),
            annots.join(" ")
        ));
        kids.push(format!("{} 0 R", objects.len()));
    }
    objects[0] = "<< /Type /Catalog /Pages 2 0 R >>".into();
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {pages} >>",
        kids.join(" ")
    );
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        write!(pdf, "{} 0 obj\n{body}\nendobj\n", i + 1).unwrap();
    }
    let xref = pdf.len();
    writeln!(pdf, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1).unwrap();
    for offset in offsets {
        writeln!(pdf, "{offset:010} 00000 n ").unwrap();
    }
    write!(
        pdf,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    )
    .unwrap();
    std::fs::write(path, pdf).unwrap();
}

/// A rebuild of a long PDF with as many pages shows without every page read first: Reload →
/// Opened takes well under half the time of an Open, and the first page comes no later. On a
/// generated 3000-page PDF; pdf50 (when built) and `LEMMA_PERF_PDF` are printed too. Measured on
/// a 500-page build at load 8: Open 20.9 ms, Reload 4.8 ms (median).
#[test]
#[ignore = "perf: run with cargo test --release -- --ignored perf"]
fn perf_reload_of_a_long_pdf() {
    let dir = temp_dir("perf-reload");
    let long = dir.join("long.pdf");
    write_long_pdf(&long, 3000);
    let mut pdfs = vec![long.clone()];
    pdfs.extend(pdf50());
    pdfs.extend(perf_pdf());
    for pdf in pdfs {
        let (handle, rx, sizes) = open_path(&pdf);
        let (mut opens, mut reloads) = (Vec::new(), Vec::new());
        for generation in 2..14 {
            let t = Instant::now();
            let path = pdf.clone();
            handle.send(if generation % 2 == 0 {
                PdfCmd::Open {
                    generation,
                    pdf: path,
                }
            } else {
                PdfCmd::Reload {
                    generation,
                    pdf: path,
                }
            });
            opened(&rx, generation);
            let shown = ms_since(t);
            let first = shown + first_of_three_pages(&handle, &rx, generation);
            let times = if generation % 2 == 0 {
                &mut opens
            } else {
                &mut reloads
            };
            times.push((shown, first));
            // The rest of the pages are read meanwhile.
            std::thread::sleep(Duration::from_millis(100));
            while rx.try_recv().is_ok() {}
        }
        let split = |times: &[(f64, f64)]| {
            let mut shown: Vec<f64> = times.iter().map(|t| t.0).collect();
            let mut first: Vec<f64> = times.iter().map(|t| t.1).collect();
            (median_max(&mut shown), median_max(&mut first))
        };
        let ((open, open_max), (open_first, _)) = split(&opens);
        let ((reload, reload_max), (reload_first, _)) = split(&reloads);
        eprintln!(
            "perf_reload_of_a_long_pdf: {} ({} pages): shown after an Open {open:.1} / \
             {open_max:.1} ms, after a Reload {reload:.1} / {reload_max:.1} ms; the first page \
             after {open_first:.1} and {reload_first:.1} ms (median / max, load {})",
            pdf.display(),
            sizes.len(),
            load_average()
        );
        if pdf == long {
            assert!(
                reload * 2.0 < open,
                "Reload {reload:.1} ms, Open {open:.1} ms"
            );
            assert!(
                reload_first <= open_first,
                "the first page after {reload_first:.1} ms, {open_first:.1} ms after an Open"
            );
        }
        assert!(handle.shutdown_and_join(TIMEOUT), "the pdf thread stops");
    }
}
