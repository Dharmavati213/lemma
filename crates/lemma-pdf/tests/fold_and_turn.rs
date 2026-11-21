//! Search that folds `ß` to `ss` and finds a word hyphenated across a page break, and links to
//! pages turned by `/Rotate`, on small PDFs written here (Helvetica, no TeX needed).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_channel::{Receiver, TryRecvError};
use lemma_pdf::{LinkDest, PdfCmd, PdfEvent, PdfHandle, SearchMatch, SearchOptions};

const TIMEOUT: Duration = Duration::from_secs(20);

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-pdf-fold-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// One page of [`write_pdf`]: its `/Rotate`, its lines of Helvetica 12 `(x, baseline, text)` in
/// PDF user space (the text a PDF string, octal escapes allowed), and its links to
/// `(page, left, top)`.
struct Page {
    rotate: u32,
    lines: Vec<(f64, f64, &'static str)>,
    links: Vec<(usize, f64, f64)>,
}

/// A PDF of letter-size `pages`. The font's codes 128 and 129 (`\u{80}`, `\u{81}` in a line) are
/// the ligatures `fi` and `fl`, which its `/ToUnicode` map makes `ﬁ` and `ﬂ` (U+FB01, U+FB02) in
/// the page's text, as some PDFs have them. Every character is written as one byte.
fn write_pdf(path: &Path, pages: &[Page]) {
    write_pdf_with_outline(path, pages, &[]);
}

/// [`write_pdf`] with an outline of `bookmarks` to `(page, left, top)`, one level.
fn write_pdf_with_outline(path: &Path, pages: &[Page], bookmarks: &[(usize, f64, f64)]) {
    // 1 catalog, 2 page tree, 3 font, 4 its ToUnicode map, then per page: the page and its
    // content stream; then the outline and its items.
    let page_obj = |i: usize| 5 + 2 * i;
    let outline_obj = page_obj(pages.len());
    let to_unicode = "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
                      /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
                      /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
                      1 begincodespacerange\n<00> <FF>\nendcodespacerange\n\
                      2 beginbfchar\n<80> <FB01>\n<81> <FB02>\nendbfchar\n\
                      endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n";
    let mut objects = vec![
        if bookmarks.is_empty() {
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned()
        } else {
            format!("<< /Type /Catalog /Pages 2 0 R /Outlines {outline_obj} 0 R >>")
        },
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            (0..pages.len())
                .map(|i| format!("{} 0 R", page_obj(i)))
                .collect::<Vec<_>>()
                .join(" "),
            pages.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type /Encoding \
         /BaseEncoding /WinAnsiEncoding /Differences [128 /fi /fl] >> /ToUnicode 4 0 R >>"
            .to_owned(),
        stream(to_unicode),
    ];
    for (i, page) in pages.iter().enumerate() {
        let annots: Vec<String> = page
            .links
            .iter()
            .enumerate()
            .map(|(k, &(to, left, top))| {
                let y = 300.0 - 20.0 * k as f64;
                format!(
                    "<< /Type /Annot /Subtype /Link /Rect [72 {y} 172 {}] /Border [0 0 0] \
                     /Dest [{} 0 R /XYZ {left} {top} null] >>",
                    y + 12.0,
                    page_obj(to)
                )
            })
            .collect();
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Rotate {} \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R /Annots [{}] >>",
            page.rotate,
            page_obj(i) + 1,
            annots.join(" ")
        ));
        let mut content = String::new();
        for (x, y, text) in &page.lines {
            writeln!(content, "BT /F1 12 Tf {x} {y} Td ({text}) Tj ET").unwrap();
        }
        objects.push(stream(&content));
    }
    if !bookmarks.is_empty() {
        let item = |k: usize| outline_obj + 1 + k;
        objects.push(format!(
            "<< /Type /Outlines /First {} 0 R /Last {} 0 R /Count {} >>",
            item(0),
            item(bookmarks.len() - 1),
            bookmarks.len()
        ));
        for (k, &(to, left, top)) in bookmarks.iter().enumerate() {
            let mut siblings = String::new();
            if k > 0 {
                write!(siblings, "/Prev {} 0 R ", item(k - 1)).unwrap();
            }
            if k + 1 < bookmarks.len() {
                write!(siblings, "/Next {} 0 R ", item(k + 1)).unwrap();
            }
            objects.push(format!(
                "<< /Title (Bookmark {k}) /Parent {outline_obj} 0 R {siblings}\
                 /Dest [{} 0 R /XYZ {left} {top} null] >>",
                page_obj(to)
            ));
        }
    }
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend(bytes(&format!("{} 0 obj\n{body}\nendobj\n", i + 1)));
    }
    let mut tail = format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        writeln!(tail, "{offset:010} 00000 n ").unwrap();
    }
    writeln!(
        tail,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
        objects.len() + 1,
        pdf.len()
    )
    .unwrap();
    pdf.extend(bytes(&tail));
    std::fs::write(path, pdf).unwrap();
}

/// A stream object holding `content`.
fn stream(content: &str) -> String {
    format!(
        "<< /Length {} >>\nstream\n{content}endstream",
        bytes(content).len()
    )
}

/// `text` as the PDF's bytes: one per character (up to U+00FF), so that `\u{80}` is the font's
/// code 128, not two bytes of UTF-8.
fn bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).expect("a character up to U+00FF"))
        .collect()
}

fn open(pdf: &Path) -> (PdfHandle, Receiver<PdfEvent>, Vec<(f64, f64)>) {
    let (tx, rx) = async_channel::unbounded();
    let handle = lemma_pdf::spawn(tx);
    handle.send(PdfCmd::Open {
        generation: 1,
        pdf: pdf.to_path_buf(),
    });
    match next(&rx) {
        PdfEvent::Opened { page_sizes, .. } => (handle, rx, page_sizes),
        other => panic!("expected Opened, got {other:?}"),
    }
}

/// Every match of `query`, by page.
fn search(
    handle: &PdfHandle,
    rx: &Receiver<PdfEvent>,
    id: u64,
    query: &str,
    options: SearchOptions,
) -> Vec<(u32, SearchMatch)> {
    handle.send(PdfCmd::Search {
        id,
        query: query.to_owned(),
        options,
        start: 0,
    });
    let mut found = Vec::new();
    loop {
        match next(rx) {
            PdfEvent::SearchPage {
                id: i,
                page,
                matches,
                ..
            } if i == id => found.extend(matches.into_iter().map(|m| (page, m))),
            PdfEvent::SearchDone { id: i, .. } if i == id => break,
            _ => {}
        }
    }
    found.sort_by_key(|(page, _)| *page);
    found
}

const ANY: SearchOptions = SearchOptions {
    case_sensitive: false,
    whole_words: false,
};
const EXACT: SearchOptions = SearchOptions {
    case_sensitive: true,
    whole_words: false,
};

#[test]
fn sharp_s_is_ss_and_ligatures_are_letters() {
    let dir = temp_dir("sharp-s");
    let pdf = dir.join("main.pdf");
    write_pdf(
        &pdf,
        &[Page {
            rotate: 0,
            lines: vec![
                (72.0, 700.0, r"Die Stra\337e, die STRASSE und die Strasse."),
                (72.0, 680.0, "We \u{80}nd the \u{81}ow."),
            ],
            links: Vec::new(),
        }],
    );
    let (handle, rx, _) = open(&pdf);
    let count = |id, query: &str, options| search(&handle, &rx, id, query, options).len();
    assert_eq!(count(1, "strasse", ANY), 3, "Straße, STRASSE and Strasse");
    assert_eq!(count(2, "Straße", ANY), 3);
    assert_eq!(count(3, "STRAßE", ANY), 3);
    assert_eq!(count(4, "Straße", EXACT), 1);
    assert_eq!(count(5, "Strasse", EXACT), 1);
    // The page's text has the ligatures `ﬁ` and `ﬂ`: found by their letters.
    assert_eq!(count(6, "find", ANY), 1);
    assert_eq!(count(7, "the flow", ANY), 1);
    assert_eq!(count(9, "FLOW", EXACT), 0);
    assert_eq!(count(10, "flow", EXACT), 1);
    // The match of "Straße" covers its six letters, not more.
    let found = search(&handle, &rx, 8, "strasse", ANY);
    let (sharp, upper) = (found[0].1[0], found[1].1[0]);
    assert!(
        sharp.2 > 20.0 && sharp.2 < upper.2,
        "{sharp:?} vs {upper:?}"
    );
    assert!(handle.shutdown_and_join(TIMEOUT));
}

#[test]
fn a_word_hyphenated_across_a_page_break_is_found_where_it_starts() {
    let dir = temp_dir("page-break");
    let pdf = dir.join("main.pdf");
    let running_head = (72.0, 750.0, "Running head");
    write_pdf(
        &pdf,
        &[
            Page {
                rotate: 0,
                lines: vec![
                    running_head,
                    (72.0, 700.0, "The first line of the text."),
                    (72.0, 100.0, "A long line that ends the page with a contra-"),
                    (300.0, 40.0, "1"),
                ],
                links: Vec::new(),
            },
            Page {
                rotate: 0,
                lines: vec![
                    running_head,
                    (72.0, 700.0, "variant functor on the next page."),
                    (300.0, 40.0, "2"),
                ],
                links: Vec::new(),
            },
        ],
    );
    let (handle, rx, sizes) = open(&pdf);
    for (id, query, options) in [
        (1, "contravariant", ANY),
        (2, "a contravariant functor", ANY),
        (
            3,
            "contravariant",
            SearchOptions {
                case_sensitive: true,
                whole_words: true,
            },
        ),
    ] {
        let found = search(&handle, &rx, id, query, options);
        assert_eq!(found.len(), 1, "{query:?}: {found:?}");
        let (page, ref m) = found[0];
        assert_eq!(page, 0, "{query:?}: drawn where it starts");
        // One rectangle, on the last line of the first page (baseline 100 bp from the bottom).
        assert_eq!(m.len(), 1, "{m:?}");
        let (x, y, w, h) = m[0];
        let line_y = sizes[0].1 - 100.0;
        assert!(y < line_y && y + h > line_y - 5.0, "{m:?}");
        assert!(x > 200.0 && x + w < 400.0, "{m:?}");
    }
    // Not a whole word with more letters around it, and not across other page ends.
    let whole = SearchOptions {
        case_sensitive: false,
        whole_words: true,
    };
    assert!(search(&handle, &rx, 4, "contravarian", whole).is_empty());
    assert!(search(&handle, &rx, 5, "head1", ANY).is_empty());
    // The halves are still found on their own pages.
    let pages = |id, query| -> Vec<u32> {
        search(&handle, &rx, id, query, ANY)
            .iter()
            .map(|(p, _)| *p)
            .collect()
    };
    assert_eq!(pages(6, "contra"), [0]);
    assert_eq!(pages(7, "variant"), [1]);
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// A link to a point of a page turned a quarter by `/Rotate` scrolls to that point's height on
/// the page as displayed, where the target text starts; the page's `top` is across it.
#[test]
fn links_land_on_their_target_on_turned_pages() {
    let dir = temp_dir("rotated-links");
    let pdf = dir.join("main.pdf");
    let target = |rotate| Page {
        rotate,
        lines: vec![(100.0, 390.0, "Target word here, and more words after it.")],
        links: Vec::new(),
    };
    write_pdf(
        &pdf,
        &[
            Page {
                rotate: 0,
                lines: vec![(72.0, 700.0, "Links.")],
                links: vec![(1, 100.0, 400.0), (2, 100.0, 400.0), (3, 100.0, 400.0)],
            },
            target(0),
            target(90),
            target(270),
        ],
    );
    let (handle, rx, sizes) = open(&pdf);
    assert_eq!(sizes[2], (792.0, 612.0));
    handle.send(PdfCmd::Links { page: 0 });
    let links = match next(&rx) {
        PdfEvent::Links { links, .. } => links,
        other => panic!("expected Links, got {other:?}"),
    };
    let found = search(&handle, &rx, 1, "Target", ANY);
    for (page, expected) in [(1u32, 392.0), (2, 100.0), (3, 512.0)] {
        let y = links
            .iter()
            .find_map(|l| match l.dest {
                LinkDest::Page { page: p, y_bp } if p == page => y_bp,
                _ => None,
            })
            .unwrap_or_else(|| panic!("a link to page {page} with a height: {links:?}"));
        assert!(
            (y - expected).abs() < 0.01,
            "page {page}: {y} (expected {expected})"
        );
        // On the target word, or just above it.
        let (_, m) = found.iter().find(|(p, _)| *p == page).expect("the target");
        let (_, wy, _, wh) = m[0];
        assert!(
            y > wy - 12.0 && y < wy + wh + 0.5,
            "page {page}: {y} vs {m:?}"
        );
    }
    assert!(handle.shutdown_and_join(TIMEOUT));
}

/// Bookmarks to points of pages turned a quarter by `/Rotate` land where links to them do, and a
/// long outline to such pages comes quickly: the direction of each page's turn, which is read
/// from its text, is read once per page, not once per bookmark (4000 text extractions took
/// 1.6 s here; the 20 needed take about 10 ms).
#[test]
fn the_outline_of_turned_pages_lands_on_its_targets_at_once() {
    let dir = temp_dir("rotated-outline");
    let pdf = dir.join("main.pdf");
    const LINE: &str = "A line of text on a page turned for display, long enough to count.";
    let lines: Vec<_> = (0..40)
        .map(|k| (100.0, 390.0 - 8.0 * f64::from(k), LINE))
        .collect();
    let mut pages = vec![Page {
        rotate: 0,
        lines: vec![(72.0, 700.0, "Bookmarks.")],
        links: Vec::new(),
    }];
    pages.extend((0..20).map(|i| Page {
        rotate: if i % 2 == 0 { 90 } else { 270 },
        lines: lines.clone(),
        links: Vec::new(),
    }));
    let bookmarks: Vec<_> = (0..4000).map(|k| (1 + k % 20, 100.0, 400.0)).collect();
    write_pdf_with_outline(&pdf, &pages, &bookmarks);
    let (handle, rx, _) = open(&pdf);
    let start = Instant::now();
    handle.send(PdfCmd::Outline);
    let items = loop {
        if let PdfEvent::Outline { items, .. } = next(&rx) {
            break items;
        }
    };
    let took = start.elapsed();
    assert_eq!(items.len(), 4000);
    for (k, item) in items.iter().enumerate() {
        // As the links of `links_land_on_their_target_on_turned_pages`.
        let page = 1 + k % 20;
        let expected = if k % 2 == 0 { 100.0 } else { 512.0 };
        match item.dest {
            LinkDest::Page {
                page: p,
                y_bp: Some(y),
            } if p as usize == page && (y - expected).abs() < 0.01 => {}
            ref other => panic!("bookmark {k}: {other:?}, expected page {page} at {expected}"),
        }
    }
    assert!(took < Duration::from_millis(500), "{took:?}");
    assert!(handle.shutdown_and_join(TIMEOUT));
}
