//! The M3 additions of the PDF worker (WP3.4): search (against `pdftotext`, streaming order,
//! latest wins, cancellation, taking turns with renders), the outline (golden), page labels and
//! thumbnails.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use async_channel::{Receiver, TryRecvError};
use lemma_pdf::{
    LinkDest, OutlineItem, PageKey, PdfCmd, PdfEvent, PdfHandle, RenderReq, SearchMatch,
    SearchOptions, ThumbReq,
};

const TIMEOUT: Duration = Duration::from_secs(30);

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(rel: &str) -> PathBuf {
    lemma_core::canonicalize(repo().join("tests/fixtures").join(rel))
        .unwrap_or_else(|e| panic!("fixture {rel}: {e}"))
}

/// `target/fixtures/pdf50.pdf`, if `scripts/build-fixtures.sh` has run.
fn pdf50() -> Option<PathBuf> {
    let pdf = repo().join("target/fixtures/pdf50.pdf");
    if pdf.is_file() {
        Some(pdf)
    } else {
        println!(
            "SKIP: {} is missing; run scripts/build-fixtures.sh",
            pdf.display()
        );
        None
    }
}

fn try_next(rx: &Receiver<PdfEvent>, timeout: Duration) -> Option<PdfEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(event) => return Some(event),
            Err(TryRecvError::Empty) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_micros(200));
            }
            Err(TryRecvError::Empty) => return None,
            Err(e) => panic!("the pdf thread is gone: {e:?}"),
        }
    }
}

fn next(rx: &Receiver<PdfEvent>) -> PdfEvent {
    try_next(rx, TIMEOUT).expect("an event from the pdf thread")
}

/// Spawns a worker and opens `pdf` as `generation`; returns the page sizes.
fn open(pdf: &Path, generation: u64) -> (PdfHandle, Receiver<PdfEvent>, Vec<(f64, f64)>) {
    let (tx, rx) = async_channel::unbounded();
    let handle = lemma_pdf::spawn(tx);
    handle.send(PdfCmd::Open {
        generation,
        pdf: pdf.to_path_buf(),
    });
    match next(&rx) {
        PdfEvent::Opened {
            generation: g,
            page_sizes,
        } if g == generation => (handle, rx, page_sizes),
        other => panic!("expected Opened, got {other:?}"),
    }
}

fn search(id: u64, query: &str, options: SearchOptions, start: u32) -> PdfCmd {
    PdfCmd::Search {
        id,
        query: query.into(),
        options,
        start,
    }
}

/// The pages of search `id` in the order they arrive, until its `SearchDone`.
fn collect_search(rx: &Receiver<PdfEvent>, id: u64) -> Vec<(u32, Vec<SearchMatch>)> {
    let mut pages = Vec::new();
    loop {
        match next(rx) {
            PdfEvent::SearchPage {
                id: i,
                page,
                matches,
                ..
            } if i == id => pages.push((page, matches)),
            PdfEvent::SearchDone { id: i, .. } if i == id => return pages,
            PdfEvent::SearchPage { .. } | PdfEvent::SearchDone { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn total(pages: &[(u32, Vec<SearchMatch>)]) -> usize {
    pages.iter().map(|(_, m)| m.len()).sum()
}

/// The text of every page of `pdf` as `pdftotext` extracts it, normalised the way a search
/// that spans lines reads it: a hyphen ending a line joins the lines, other line breaks are
/// spaces, ligatures are letters.
fn pdftotext_pages(pdf: &Path) -> Option<Vec<String>> {
    let output = Command::new("pdftotext").arg(pdf).arg("-").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout)
        .replace('\u{fb00}', "ff")
        .replace('\u{fb01}', "fi")
        .replace('\u{fb02}', "fl")
        .replace('\u{fb03}', "ffi")
        .replace('\u{fb04}', "ffl");
    let mut pages: Vec<String> = text
        .split('\u{c}')
        .map(|page| page.replace("-\n", "").replace('\n', " "))
        .collect();
    // The text ends with a form feed after the last page.
    if pages.last().is_some_and(|p| p.trim().is_empty()) {
        pages.pop();
    }
    Some(pages)
}

/// Non-overlapping occurrences of `needle` in `hay`, ignoring ASCII case; with `whole`, only
/// those not touching a letter or digit on either side.
fn count(hay: &str, needle: &str, whole: bool) -> usize {
    let (hay, needle) = (hay.to_lowercase(), needle.to_lowercase());
    let mut n = 0;
    let mut from = 0;
    while let Some(i) = hay[from..].find(&needle) {
        let at = from + i;
        let end = at + needle.len();
        let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
        if !whole || (!word(hay[..at].chars().next_back()) && !word(hay[end..].chars().next())) {
            n += 1;
        }
        from = end;
    }
    n
}

/// Acceptance: the search finds exactly what `pdftotext` extracts, page by page, on pdf50.
#[test]
fn search_counts_match_pdftotext_on_pdf50() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let Some(text) = pdftotext_pages(&pdf) else {
        println!("SKIP: pdftotext is not available");
        return;
    };
    let (handle, rx, sizes) = open(&pdf, 1);
    assert_eq!(text.len(), sizes.len());
    let mut id = 0;
    for (query, whole) in [
        ("lorem", false),
        ("dolor", false),
        ("nulla", false),
        ("Section", false),
        ("sit amet", false),
        ("ut", false),
        ("ut", true),
        ("adipiscing elit", false),
        ("equation", false),
    ] {
        id += 1;
        let options = SearchOptions {
            case_sensitive: false,
            whole_words: whole,
        };
        handle.send(search(id, query, options, 0));
        let pages = collect_search(&rx, id);
        let mut per_page = vec![0; sizes.len()];
        for (page, matches) in &pages {
            per_page[*page as usize] = matches.len();
            for m in matches {
                assert!(!m.is_empty());
                let (w, h) = sizes[*page as usize];
                for &(x, y, mw, mh) in m {
                    assert!(x >= 0.0 && y >= 0.0 && mw > 0.0 && mh > 0.0, "{m:?}");
                    assert!(x + mw <= w + 0.5 && y + mh <= h + 0.5, "{m:?} on {w}x{h}");
                }
            }
        }
        let expected: Vec<usize> = text.iter().map(|t| count(t, query, whole)).collect();
        let found = total(&pages);
        println!(
            "{query:?}{}: {found} matches",
            if whole { " (whole words)" } else { "" }
        );
        assert!(found > 0, "{query:?} occurs in pdf50");
        assert_eq!(per_page, expected, "{query:?}: matches per page");
    }
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// Case: insensitive by default, exact on request.
#[test]
fn search_is_case_insensitive_by_default() {
    let (handle, rx, _) = open(&fixture("pdf/test.pdf"), 1);
    handle.send(search(1, "LOREM", SearchOptions::default(), 0));
    let insensitive = total(&collect_search(&rx, 1));
    let exact = SearchOptions {
        case_sensitive: true,
        whole_words: false,
    };
    handle.send(search(2, "LOREM", exact, 0));
    assert_eq!(total(&collect_search(&rx, 2)), 0);
    handle.send(search(3, "Lorem", exact, 0));
    let lorem = total(&collect_search(&rx, 3));
    assert!(
        lorem > 0 && insensitive >= lorem,
        "{insensitive} vs {lorem}"
    );
    // Blank queries match nothing and finish at once.
    handle.send(search(4, "  ", SearchOptions::default(), 0));
    assert!(collect_search(&rx, 4).is_empty());
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// Coordinates are in bp from the top-left corner: the section heading lies above the text.
#[test]
fn search_rectangles_are_top_left_bp() {
    let (handle, rx, sizes) = open(&fixture("pdf/test.pdf"), 1);
    let first = |id: u64, query: &str| {
        handle.send(search(id, query, SearchOptions::default(), 0));
        let pages = collect_search(&rx, id);
        let (page, matches) = pages.first().expect("a match").clone();
        (page, matches[0][0])
    };
    let (p1, heading) = first(1, "Sheaves");
    let (p2, text) = first(2, "Pellentesque habitant");
    assert_eq!((p1, p2), (0, 0));
    println!("heading {heading:?}, text {text:?}, page {:?}", sizes[0]);
    assert!(
        heading.1 < text.1,
        "the heading is above the first paragraph"
    );
    assert!(heading.1 > 50.0 && heading.1 < sizes[0].1 / 3.0);
    // About one line of 10 pt type.
    assert!(heading.3 > 5.0 && heading.3 < 20.0, "{heading:?}");
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// Results stream from the start page on, wrapping around, and end with `SearchDone`.
#[test]
fn search_streams_from_the_start_page_and_wraps() {
    let (handle, rx, sizes) = open(&fixture("pdf/stress.pdf"), 1);
    let n = sizes.len() as u32;
    handle.send(search(1, "e", SearchOptions::default(), 0));
    let all = collect_search(&rx, 1);
    let with_matches: Vec<u32> = all.iter().map(|(p, _)| *p).collect();
    assert!(with_matches.len() > 3, "{with_matches:?}");
    assert!(with_matches.windows(2).all(|w| w[0] < w[1]));
    let start = with_matches[with_matches.len() / 2];
    handle.send(search(2, "e", SearchOptions::default(), start));
    let wrapped: Vec<u32> = collect_search(&rx, 2).iter().map(|(p, _)| *p).collect();
    let expected: Vec<u32> = with_matches
        .iter()
        .copied()
        .filter(|p| *p >= start)
        .chain(with_matches.iter().copied().filter(|p| *p < start))
        .collect();
    assert_eq!(wrapped, expected);
    // A start past the end is the last page.
    handle.send(search(3, "e", SearchOptions::default(), n + 10));
    let pages = collect_search(&rx, 3);
    assert_eq!(total(&pages), total(&all));
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// Latest wins: a new search replaces the one in progress, which gets no `SearchDone`.
#[test]
fn a_new_search_replaces_the_running_one() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let (handle, rx, _) = open(&pdf, 1);
    handle.send(search(1, "dolor", SearchOptions::default(), 0));
    handle.send(search(2, "lorem", SearchOptions::default(), 0));
    let mut events = Vec::new();
    loop {
        let event = next(&rx);
        let done = matches!(event, PdfEvent::SearchDone { id: 2, .. });
        events.push(event);
        if done {
            break;
        }
    }
    let first_new = events
        .iter()
        .position(|e| {
            matches!(
                e,
                PdfEvent::SearchPage { id: 2, .. } | PdfEvent::SearchDone { id: 2, .. }
            )
        })
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, PdfEvent::SearchDone { id: 1, .. })),
        "the replaced search does not finish"
    );
    assert!(
        events[first_new..]
            .iter()
            .all(|e| !matches!(e, PdfEvent::SearchPage { id: 1, .. })),
        "nothing of the old search after the new one started"
    );
    let lorem: usize = events
        .iter()
        .map(|e| match e {
            PdfEvent::SearchPage { id: 2, matches, .. } => matches.len(),
            _ => 0,
        })
        .sum();
    handle.send(search(3, "lorem", SearchOptions::default(), 0));
    assert_eq!(lorem, total(&collect_search(&rx, 3)));
    assert!(try_next(&rx, Duration::from_millis(200)).is_none());
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// `CancelSearch` stops the search: no more pages, no `SearchDone`.
#[test]
fn cancel_stops_the_search() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let (handle, rx, _) = open(&pdf, 1);
    handle.send(search(1, "e", SearchOptions::default(), 0));
    handle.send(PdfCmd::CancelSearch);
    let started = Instant::now();
    let mut pages = 0;
    while let Some(event) = try_next(&rx, Duration::from_millis(300)) {
        match event {
            PdfEvent::SearchPage { id: 1, .. } => pages += 1,
            other => panic!("unexpected after a cancel: {other:?}"),
        }
    }
    println!("{pages} pages reported before the cancel took effect");
    assert!(pages < 50, "the search was stopped");
    assert!(started.elapsed() < Duration::from_secs(5));
    // The worker still serves new searches.
    handle.send(search(2, "lorem", SearchOptions::default(), 0));
    assert!(total(&collect_search(&rx, 2)) > 0);
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// Searching and rendering take turns: a render request is not delayed until the search ends.
#[test]
fn renders_and_searches_take_turns() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let (handle, rx, _) = open(&pdf, 1);
    handle.send(search(1, "e", SearchOptions::default(), 0));
    handle.send(PdfCmd::Want(
        (0..3)
            .map(|page| RenderReq {
                key: PageKey {
                    generation: 1,
                    page,
                    scale_q: 8,
                    tile: None,
                    dark: false,
                },
            })
            .collect(),
    ));
    let mut order = Vec::new();
    loop {
        match next(&rx) {
            PdfEvent::Rendered(r) => order.push(format!("r{}", r.key.page)),
            PdfEvent::SearchPage { page, .. } => order.push(format!("s{page}")),
            PdfEvent::SearchDone { .. } => {
                order.push("done".into());
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    let last_render = order.iter().rposition(|e| e.starts_with('r')).unwrap();
    println!("{}", order.join(" "));
    assert_eq!(order.iter().filter(|e| e.starts_with('r')).count(), 3);
    assert!(last_render < 8, "the renders came early: {order:?}");
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// A search of a document that was replaced meanwhile is dropped; the new one can be searched.
#[test]
fn reopening_drops_the_search_of_the_old_document() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let (handle, rx, _) = open(&pdf, 1);
    handle.send(search(1, "e", SearchOptions::default(), 0));
    handle.send(PdfCmd::Open {
        generation: 2,
        pdf: fixture("pdf/test.pdf"),
    });
    let mut opened = false;
    while let Some(event) = try_next(&rx, Duration::from_millis(300)) {
        match event {
            PdfEvent::Opened { generation: 2, .. } => opened = true,
            PdfEvent::SearchPage {
                id: 1, generation, ..
            } => {
                assert!(!opened && generation == 1)
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(opened);
    handle.send(search(2, "lorem", SearchOptions::default(), 0));
    let pages = collect_search(&rx, 2);
    assert!(total(&pages) > 0);
    // Without a document, a search is done at once.
    let (tx, rx2) = async_channel::unbounded();
    let empty = lemma_pdf::spawn(tx);
    empty.send(search(9, "x", SearchOptions::default(), 0));
    assert!(matches!(
        next(&rx2),
        PdfEvent::SearchDone {
            id: 9,
            generation: 0
        }
    ));
    assert!(empty.shutdown_and_join(TIMEOUT));
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// The outline as text: one line per entry, indented by level, with its destination.
fn outline_text(items: &[OutlineItem]) -> String {
    fn walk(out: &mut String, items: &[OutlineItem], depth: usize) {
        for item in items {
            let dest = match &item.dest {
                LinkDest::Page { page, y_bp } => match y_bp {
                    Some(y) => format!("page {} y {:.0}", page + 1, y),
                    None => format!("page {}", page + 1),
                },
                LinkDest::Uri(uri) => format!("uri {uri}"),
                LinkDest::Other => "other".into(),
            };
            let open = if item.open { " (open)" } else { "" };
            writeln!(out, "{}{} -> {dest}{open}", "  ".repeat(depth), item.title).unwrap();
            walk(out, &item.children, depth + 1);
        }
    }
    let mut out = String::new();
    walk(&mut out, items, 0);
    out
}

fn outline_of(pdf: &Path) -> Vec<OutlineItem> {
    let (handle, rx, _) = open(pdf, 7);
    handle.send(PdfCmd::Outline);
    let items = match next(&rx) {
        PdfEvent::Outline {
            generation: 7,
            items,
        } => items,
        other => panic!("expected Outline, got {other:?}"),
    };
    assert!(handle.shutdown_and_join(TIMEOUT));
    items
}

/// Acceptance: the bookmark tree of `proj` (hyperref) matches the golden file; PDFs without
/// bookmarks have an empty outline. `UPDATE_GOLDEN=1` rewrites the golden file.
#[test]
fn outline_of_proj_matches_the_golden_tree() {
    let text = outline_text(&outline_of(&fixture("proj/main.pdf")));
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/proj-outline.txt");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &text).unwrap();
    }
    print!("{text}");
    let expected = std::fs::read_to_string(&golden).expect("the golden outline");
    assert_eq!(text, expected);
    assert!(outline_of(&fixture("pdf/stress.pdf")).is_empty());
    // Without a document: generation 0 and no items.
    let (tx, rx) = async_channel::unbounded();
    let handle = lemma_pdf::spawn(tx);
    handle.send(PdfCmd::Outline);
    handle.send(PdfCmd::PageLabels);
    assert!(matches!(
        next(&rx),
        PdfEvent::Outline {
            generation: 0,
            ref items
        } if items.is_empty()
    ));
    assert!(matches!(
        next(&rx),
        PdfEvent::PageLabels {
            generation: 0,
            ref labels
        } if labels.is_empty()
    ));
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// pdf50 has one section per page, so one bookmark per page, each pointing at its page.
#[test]
fn outline_of_pdf50_has_a_bookmark_per_page() {
    let Some(pdf) = pdf50() else {
        return;
    };
    let items = outline_of(&pdf);
    assert_eq!(items.len(), 50);
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item.title, format!("Section {}", i + 1));
        assert!(
            matches!(item.dest, LinkDest::Page { page, y_bp: Some(y) } if page == i as u32 && y < 200.0),
            "{item:?}"
        );
        assert!(item.children.is_empty());
    }
}

/// Page labels: the PDF's own (roman front matter), else the page numbers.
#[test]
fn page_labels_follow_the_pdf() {
    let labels = |pdf: &Path| {
        let (handle, rx, _) = open(pdf, 3);
        handle.send(PdfCmd::PageLabels);
        let labels = match next(&rx) {
            PdfEvent::PageLabels {
                generation: 3,
                labels,
            } => labels,
            other => panic!("expected PageLabels, got {other:?}"),
        };
        assert!(handle.shutdown_and_join(TIMEOUT));
        labels
    };
    assert_eq!(labels(&fixture("proj/main.pdf")), ["1", "2", "3", "4", "5"]);
    let have_pdflatex = Command::new("pdflatex")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !have_pdflatex {
        println!("SKIP: pdflatex is not installed");
        return;
    }
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-pdf-m3-{}-labels", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("labels.tex"),
        "\\documentclass{article}\\usepackage{hyperref}\\begin{document}\
         \\pagenumbering{roman}a\\newpage b\\newpage\\pagenumbering{arabic}c\\newpage d\
         \\end{document}\n",
    )
    .unwrap();
    let built = Command::new("pdflatex")
        .args(["-interaction=nonstopmode", "-halt-on-error", "labels.tex"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stdout)
    );
    assert_eq!(labels(&dir.join("labels.pdf")), ["i", "ii", "1", "2"]);
}

/// Thumbnails come at the requested width with the page's aspect; requests for other
/// generations or missing pages are skipped; a new list replaces the pending one.
#[test]
fn thumbnails_render_at_the_requested_width() {
    let (handle, rx, sizes) = open(&fixture("pdf/stress.pdf"), 4);
    let req = |generation, page, width| ThumbReq {
        generation,
        page,
        width,
        dark: false,
    };
    handle.send(PdfCmd::WantThumbnails(vec![
        req(3, 0, 100),
        req(4, 99, 100),
        req(4, 0, 0),
        req(4, 2, 120),
        req(4, 5, 200),
    ]));
    for want in [req(4, 2, 120), req(4, 5, 200)] {
        match next(&rx) {
            PdfEvent::Thumbnail(t) => {
                assert_eq!(t.req, want);
                let (w, h) = sizes[want.page as usize];
                assert_eq!(t.width, i32::from(want.width));
                assert_eq!(t.height, (h * f64::from(want.width) / w).round() as i32);
                assert_eq!(t.data.len(), t.stride as usize * t.height as usize);
                assert_eq!(&t.data[..4], &[255, 255, 255, 255], "a white page");
                assert!(
                    t.data.as_chunks::<4>().0.iter().any(|px| px[0] < 160),
                    "with ink"
                );
            }
            other => panic!("expected a thumbnail, got {other:?}"),
        }
    }
    assert!(try_next(&rx, Duration::from_millis(200)).is_none());
    // Latest wins: of 20 requests only the first few can be served before the new list.
    handle.send(PdfCmd::WantThumbnails(
        (0..20).map(|p| req(4, p, 300)).collect(),
    ));
    handle.send(PdfCmd::WantThumbnails(vec![req(4, 22, 64)]));
    let mut got = Vec::new();
    while let Some(event) = try_next(&rx, Duration::from_millis(300)) {
        match event {
            PdfEvent::Thumbnail(t) => got.push(t.req),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(got.last(), Some(&req(4, 22, 64)));
    assert!(got.len() < 20, "{got:?}");
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// A dark thumbnail is the light one through the dark-mode transform, as a dark page bitmap is.
#[test]
fn dark_thumbnails_come_back_coloured_for_dark_mode() {
    let (handle, rx, _) = open(&fixture("pdf/stress.pdf"), 1);
    let light = ThumbReq {
        generation: 1,
        page: 2,
        width: 120,
        dark: false,
    };
    let dark = ThumbReq {
        dark: true,
        ..light
    };
    handle.send(PdfCmd::WantThumbnails(vec![light, dark]));
    let thumbnail = || match next(&rx) {
        PdfEvent::Thumbnail(t) => t,
        other => panic!("expected a thumbnail, got {other:?}"),
    };
    let (plain, darkened) = (thumbnail(), thumbnail());
    assert_eq!((plain.req, darkened.req), (light, dark));
    assert_eq!(
        (darkened.width, darkened.height),
        (plain.width, plain.height)
    );
    let mut expected = plain.data.to_vec();
    lemma_pdf::dark::darken_bgra(&mut expected);
    assert!(expected[..] == darkened.data[..]);
    let paper = (lemma_pdf::dark::PAPER * 255.0).round() as u8;
    assert_eq!(&darkened.data[..4], &[paper, paper, paper, 255]);
    assert!(handle.shutdown_and_join(TIMEOUT));
}
