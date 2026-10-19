//! The preview pipeline with real TeX: formulas of the fixture project, user macros, errors and
//! timeouts, the format's life cycle, the fallbacks and the latency.
//!
//! Every test uses its own cache directory and copy of the fixture under `target/`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves the rendered formulas there as PNGs.
//! The latency test: `cargo test --release -p lemma-preview --test pipeline -- --ignored perf`.

use lemma_build::Engine;
use lemma_core::LineIndex;
use lemma_preview::{
    DEFAULT_PREAMBLE, FormulaKind, Image, Job, Outcome, Pixels, Point, PreambleSource,
    PreviewError, Previewer, Route, definitions, find_formula, project_key, split_preamble,
};
use lemma_syntax::{LineStates, ScanConfig, StrLines};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/proj");

/// A fresh directory under cargo's per-test temp dir.
fn temp(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-preview-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A copy of the fixture project (sources only) in a fresh directory, and a cache directory.
fn project(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp(name);
    let proj = dir.join("proj");
    fs::create_dir_all(proj.join("chapters")).unwrap();
    for file in ["main.tex", "refs.bib", "chapters/yoneda.tex"] {
        fs::copy(Path::new(FIXTURE).join(file), proj.join(file)).unwrap();
    }
    (proj, dir.join("cache"))
}

fn tex_available() -> bool {
    let ok = Command::new("kpsewhich")
        .arg("mylatexformat.ltx")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        println!("SKIP: pdflatex or mylatexformat is not installed");
    }
    ok
}

/// The formula at the first `needle` (+ `delta`) of `text`.
fn formula_at(text: &str, needle: &str, delta: usize) -> lemma_preview::Formula {
    let index = LineIndex::new(text);
    let mut states = LineStates::new(Arc::new(ScanConfig::default()));
    let lines = StrLines {
        text,
        index: &index,
    };
    states.rebuild(&lines);
    let at = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} in the text"))
        + delta;
    let (line, col) = index.position(at);
    find_formula(
        &states,
        &lines,
        Point::new(line, col),
        &["tikzcd".to_owned()],
    )
    .unwrap_or_else(|| panic!("a formula at {needle:?}"))
}

/// A job for the formula at `needle` in `file` of the project whose root is `root`, with the
/// root's live preamble (and the definitions of its body and of `file`).
fn job(root: &Path, file: &Path, needle: &str) -> Job {
    let root_text = fs::read_to_string(root).unwrap();
    let text = fs::read_to_string(file).unwrap();
    let (preamble, body) = split_preamble(&root_text).unwrap();
    let mut defs = definitions(body);
    if file != root {
        defs.push_str(&definitions(&text));
    }
    let f = formula_at(&text, needle, 0);
    Job {
        root: Some(root.to_path_buf()),
        dir: file.parent().unwrap().to_path_buf(),
        preamble: PreambleSource::Text(preamble.into()),
        definitions: defs,
        kind: f.kind,
        formula: f.text,
        scale: 2.0,
    }
}

fn image(outcome: &Outcome) -> &Image {
    match &outcome.result {
        Ok(image) => image,
        Err(e) => panic!("no image: {e} ({outcome:?})"),
    }
}

fn snapshot(name: &str, image: &Image) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let (w, h) = (image.width as i32, image.height as i32);
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().unwrap();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let px = &mut data[y * stride + x * 4..y * stride + x * 4 + 4];
                match &image.pixels {
                    // Black on white.
                    Pixels::Alpha(a) => {
                        let v = 255 - a[y * w as usize + x];
                        px.copy_from_slice(&[v, v, v, 255]);
                    }
                    Pixels::Bgra(p) => {
                        let s = &p[(y * w as usize + x) * 4..(y * w as usize + x) * 4 + 4];
                        let a = u32::from(s[3]);
                        for c in 0..3 {
                            px[c] = (u32::from(s[c]) + (255 - a)) as u8;
                        }
                        px[3] = 255;
                    }
                }
            }
        }
    }
    let path = Path::new(&dir).join(format!("{name}.png"));
    let mut file = fs::File::create(&path).unwrap();
    surface.write_to_png(&mut file).unwrap();
}

/// Coverage of the mask summed per column: a fingerprint of what was typeset.
fn ink(image: &Image) -> u64 {
    match &image.pixels {
        Pixels::Alpha(a) => a.iter().map(|&v| u64::from(v)).sum(),
        Pixels::Bgra(p) => p.chunks(4).map(|c| u64::from(c[3])).sum(),
    }
}

#[test]
fn fixture_formulas_inline_display_align_and_tikzcd() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("fixture");
    let main = proj.join("main.tex");
    // An `align*` variant of the fixture's `align`.
    let text = fs::read_to_string(&main).unwrap();
    fs::write(
        &main,
        text.replacen("\\begin{align}", "\\begin{align*}", 1)
            .replacen("\\end{align}", "\\end{align*}", 1),
    )
    .unwrap();
    let previewer = Previewer::new(&cache);
    let cases = [
        ("inline", main.clone(), "\\cO_X"),
        ("display", main.clone(), "\\mathrm{QCoh}(S)\\longrightarrow"),
        ("align-star", main.clone(), "\\lim_{U\\in\\cC}"),
        ("equation", main.clone(), "p_{13}^*\\varphi"),
        (
            "tikzcd",
            proj.join("chapters/yoneda.tex"),
            "\\arrow[r, \"\\eta_X\"]",
        ),
    ];
    let mut sizes = Vec::new();
    for (name, file, needle) in cases {
        let job = job(&main, &file, needle);
        let outcome = previewer.render(&job);
        let img = image(&outcome);
        snapshot(name, img);
        assert_eq!(outcome.route, Some(Route::Format), "{name}");
        assert_eq!(outcome.engine, Engine::PdfLatex);
        assert!(img.is_mask(), "{name}: black ink is a mask");
        assert!(ink(img) > 1000, "{name}: ink");
        sizes.push((name, img.width, img.height));
    }
    println!("sizes (px at 2 px/bp): {sizes:?}");
    let size = |n: &str| sizes.iter().find(|s| s.0 == n).map(|s| (s.1, s.2)).unwrap();
    // `$\cO_X$`: about 2 em wide and one line high (11 pt at 2 px/bp, plus margins).
    let (w, h) = size("inline");
    assert!(
        (25..80).contains(&w) && (20..50).contains(&h),
        "inline {w}×{h}"
    );
    // Three aligned lines are taller than one display line; the diagram is a 2×2 square.
    assert!(size("align-star").1 > size("display").1 + 40, "{sizes:?}");
    let (w, h) = size("tikzcd");
    assert!(w > 180 && h > 90, "tikzcd {w}×{h}");
    // One format for the five formulas, no run directory left behind.
    assert_eq!(previewer.format_builds(), 1);
    assert_eq!(previewer.tex_runs(), 5);
    let project_dir = cache.join(project_key(Some(&main)));
    assert!(project_dir.join("lemma.fmt").is_file());
    let left: Vec<String> = fs::read_dir(&project_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut left = left;
    left.sort();
    assert_eq!(left, ["lemma.fmt", "stamp"]);
}

#[test]
fn equation_numbers_are_not_drawn() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("numbers");
    let main = proj.join("main.tex");
    let previewer = Previewer::new(&cache);
    // The fixture's numbered `align` and the same formula unnumbered render alike.
    let numbered = previewer.render(&job(&main, &main, "\\lim_{U\\in\\cC}"));
    let text = fs::read_to_string(&main).unwrap();
    fs::write(
        &main,
        text.replacen("\\begin{align}", "\\begin{align*}", 1)
            .replacen("\\end{align}", "\\end{align*}", 1),
    )
    .unwrap();
    let starred = previewer.render(&job(&main, &main, "\\lim_{U\\in\\cC}"));
    let (a, b) = (image(&numbered), image(&starred));
    assert_eq!((a.width, a.height), (b.width, b.height));
    assert_eq!(ink(a), ink(b));
}

#[test]
fn user_macros_are_honoured() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("macros");
    let main = proj.join("main.tex");
    // A macro defined in the body (not in the format) and one in the chapter.
    let text = fs::read_to_string(&main).unwrap();
    fs::write(
        &main,
        text.replacen(
            "\\maketitle",
            "\\maketitle\n\\newcommand{\\bodymacro}{\\mathfrak{B}}\n\\DeclareMathOperator{\\Ext}{Ext}",
            1,
        )
        .replacen(
            "Let $X$ be a scheme",
            "Let $X$ be a scheme, $\\cF$ and $F$, $\\Hom$ and $H$, $\\Ext^1(\\bodymacro)$ and $\\undefinedmacro$",
            1,
        ),
    )
    .unwrap();
    let previewer = Previewer::new(&cache);
    let render = |needle: &str| previewer.render(&job(&main, &main, needle));
    let cf = render("\\cF$ and");
    let f = render("F$, $\\Hom");
    let hom = render("\\Hom$ and");
    let h = render("H$, $\\Ext");
    let ext = render("\\Ext^1");
    for (name, o) in [("cF", &cf), ("Hom", &hom), ("Ext", &ext)] {
        snapshot(&format!("macro-{name}"), image(o));
    }
    // \cF is \mathcal{F}, not F; \Hom is the three letters "Hom", wider than H.
    assert_ne!(ink(image(&cf)), ink(image(&f)));
    assert!(image(&hom).width > image(&h).width + 20);
    assert!(image(&ext).width > 60);
    // An unknown macro is an error that names it.
    let bad = render("\\undefinedmacro");
    match &bad.result {
        Err(PreviewError::Tex {
            message,
            context,
            line,
        }) => {
            assert_eq!(message, "Undefined control sequence");
            assert!(
                context
                    .as_deref()
                    .unwrap_or("")
                    .contains("\\undefinedmacro"),
                "{context:?}"
            );
            assert_eq!(*line, Some(1));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn chapter_definitions_count() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("chapter");
    let main = proj.join("main.tex");
    let chapter = proj.join("chapters/yoneda.tex");
    let text = fs::read_to_string(&chapter).unwrap();
    fs::write(
        &chapter,
        format!("\\newcommand{{\\yo}}{{\\mathbf{{y}}}}\n{text}\nFinally $\\yo(X)$.\n"),
    )
    .unwrap();
    let previewer = Previewer::new(&cache);
    let outcome = previewer.render(&job(&main, &chapter, "\\yo(X)"));
    assert!(image(&outcome).width > 20);
}

#[test]
fn malformed_formulas_show_their_error_and_never_hang() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("malformed");
    let main = proj.join("main.tex");
    let previewer = Previewer::new(&cache);
    // Warm up the format first: its build is not part of the timings below.
    image(&previewer.render(&job(&main, &main, "\\cO_X")));
    let doc = "\\[\n a \\frac{1}{ \n\\]\n$\\left( x$\n\\[ \\def\\loop{\\loop}\\loop \\]\n\
\\begin{align*}\n a &= b \\\\\n c &&&&= \\end{matrix}\n\\end{align*}\n";
    let job_for = |needle: &str| {
        let f = formula_at(doc, needle, 0);
        let mut j = job(&main, &main, "\\cO_X");
        j.kind = f.kind;
        j.formula = f.text;
        j
    };
    for (needle, expect) in [
        ("\\frac", "Missing } inserted"),
        ("\\left(", "Missing \\right. inserted"),
        ("c &&", "\\begin{align*} on line 1 ended by \\end{matrix}"),
    ] {
        let started = Instant::now();
        let outcome = previewer.render(&job_for(needle));
        let elapsed = started.elapsed();
        match &outcome.result {
            Err(PreviewError::Tex { message, line, .. }) => {
                assert!(message.contains(expect), "{needle}: {message}");
                assert!(line.is_some(), "{needle}: the formula's line");
            }
            other => panic!("{needle}: {other:?}"),
        }
        assert!(elapsed < Duration::from_secs(2), "{needle}: {elapsed:?}");
    }
    // An endless loop is stopped at the timeout.
    let started = Instant::now();
    let outcome = previewer.render(&job_for("\\def\\loop"));
    assert_eq!(
        outcome.result.unwrap_err(),
        PreviewError::Timeout(Duration::from_secs(2))
    );
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(3000), "{elapsed:?}");
    // No TeX process of the preview is left.
    let project_dir = cache.join(project_key(Some(&main)));
    let leftovers = fs::read_dir(&project_dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("run-")
        })
        .count();
    assert_eq!(leftovers, 0);
}

#[test]
fn the_format_is_rebuilt_only_when_the_preamble_changes() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("format");
    let main = proj.join("main.tex");
    fs::write(
        proj.join("macros.tex"),
        "\\newcommand{\\fromfile}{\\mathsf{M}}\n",
    )
    .unwrap();
    let text = fs::read_to_string(&main).unwrap();
    fs::write(
        &main,
        text.replacen("\\addbibresource", "\\input{macros}\n\\addbibresource", 1)
            .replacen("Let $X$", "Let $\\fromfile + X$", 1),
    )
    .unwrap();
    let previewer = Previewer::new(&cache);
    let first = previewer.render(&job(&main, &main, "\\fromfile"));
    assert!(first.format_built);
    let width = image(&first).width;
    // Other formulas, the same formula again, the body edited: no rebuild.
    for needle in ["\\cO_X", "\\fromfile", "\\mathrm{QCoh}"] {
        let o = previewer.render(&job(&main, &main, needle));
        assert!(!o.format_built, "{needle}");
        image(&o);
    }
    assert_eq!(previewer.format_builds(), 1);
    // The root read from disk has the same preamble: no rebuild either.
    let mut from_disk = job(&main, &main, "\\fromfile");
    from_disk.preamble = PreambleSource::Root;
    from_disk.definitions.clear();
    let o = previewer.render(&from_disk);
    assert!(!o.format_built);
    assert_eq!(o.preamble_hash, first.preamble_hash);
    // The preamble changes: rebuilt once.
    let text = fs::read_to_string(&main).unwrap();
    fs::write(
        &main,
        text.replacen(
            "\\begin{document}",
            "\\newcommand{\\Z}{\\mathbb{Z}}\n\\begin{document}",
            1,
        ),
    )
    .unwrap();
    let o = previewer.render(&job(&main, &main, "\\fromfile"));
    assert!(o.format_built);
    assert_ne!(o.preamble_hash, first.preamble_hash);
    assert!(!previewer.render(&job(&main, &main, "\\cO_X")).format_built);
    assert_eq!(previewer.format_builds(), 2);
    // A file the preamble inputs changes: rebuilt, and the new definition is used.
    std::thread::sleep(Duration::from_millis(20));
    fs::write(
        proj.join("macros.tex"),
        "\\newcommand{\\fromfile}{\\mathsf{MMMM}}\n",
    )
    .unwrap();
    let o = previewer.render(&job(&main, &main, "\\fromfile"));
    assert!(o.format_built);
    assert!(image(&o).width > width + 20, "the new macro");
    assert_eq!(previewer.format_builds(), 3);
    // Another previewer (a new session) reuses the format on disk.
    let again = Previewer::new(&cache);
    assert!(!again.render(&job(&main, &main, "\\fromfile")).format_built);
    assert_eq!(again.format_builds(), 0);
}

#[test]
fn a_preamble_that_cannot_be_dumped_falls_back_to_full_builds() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("undumpable");
    let main = proj.join("main.tex");
    let previewer = Previewer::new(&cache);
    // A missing package: no format, and the full build shows the error.
    let text = fs::read_to_string(&main).unwrap();
    fs::write(
        &main,
        text.replacen(
            "\\usepackage{tikz-cd}",
            "\\usepackage{tikz-cd}\n\\usepackage{lemma-no-such-package}",
            1,
        ),
    )
    .unwrap();
    let o = previewer.render(&job(&main, &main, "\\cO_X"));
    assert_eq!(o.route, Some(Route::Full));
    assert!(o.format_built, "it tried");
    match &o.result {
        Err(PreviewError::Tex { message, .. }) => {
            assert!(message.contains("lemma-no-such-package.sty"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // Remembered: the next hover does not try again.
    let o = previewer.render(&job(&main, &main, "\\mathrm{QCoh}"));
    assert!(!o.format_built);
    assert_eq!(o.route, Some(Route::Full));
    assert_eq!(previewer.format_builds(), 1);
}

#[test]
fn a_preamble_in_a_file_of_its_own() {
    if !tex_available() {
        return;
    }
    // `\input{preamble}` must find the project's preamble.tex, not the format build's own file.
    let dir = temp("own-file");
    fs::write(
        dir.join("preamble.tex"),
        "\\documentclass{article}\n\\usepackage{amssymb}\n\\newcommand{\\R}{\\mathbb{R}}\n",
    )
    .unwrap();
    let root = dir.join("book.tex");
    fs::write(
        &root,
        "\\input{preamble}\n\\begin{document}\nLet $f\\colon \\R \\to \\R$.\n\\end{document}\n",
    )
    .unwrap();
    let previewer = Previewer::new(dir.join("cache"));
    let o = previewer.render(&job(&root, &root, "f\\colon"));
    assert_eq!(o.route, Some(Route::Format));
    assert!(image(&o).width > 60);
}

#[test]
fn unicode_engine_documents_compile_the_whole_preamble() {
    if !tex_available() || Command::new("lualatex").arg("--version").output().is_err() {
        return;
    }
    let dir = temp("fontspec");
    let root = dir.join("thesis.tex");
    fs::write(
        &root,
        "\\documentclass{article}\n\\usepackage{fontspec}\n\\usepackage{amsmath}\n\
\\newcommand{\\cF}{\\mathcal{F}}\n\\begin{document}\nSo $\\cF \\otimes \\cF$.\n\\end{document}\n",
    )
    .unwrap();
    let previewer = Previewer::new(dir.join("cache"));
    let o = previewer.render(&job(&root, &root, "\\cF \\otimes"));
    println!("lualatex preview: {:?}", o.total_time);
    assert_eq!(o.engine, Engine::LuaLatex);
    assert_eq!(o.route, Some(Route::Full));
    assert!(!o.format_built);
    let img = image(&o);
    snapshot("lualatex", img);
    assert!(img.width > 40);
}

#[test]
fn documents_without_a_root_use_the_default_preamble() {
    if !tex_available() {
        return;
    }
    let dir = temp("noroot");
    let previewer = Previewer::new(dir.join("cache"));
    let text = "\\newcommand{\\N}{\\mathbb{N}}\nWe have $\\N \\xrightarrow{f} \\mathbb{Z}$.\n";
    let f = formula_at(text, "\\N \\x", 0);
    let o = previewer.render(&Job {
        root: None,
        dir: dir.clone(),
        preamble: PreambleSource::Default,
        definitions: definitions(text),
        kind: f.kind,
        formula: f.text,
        scale: 2.0,
    });
    assert_eq!(o.route, Some(Route::Format));
    snapshot("default-preamble", image(&o));
    assert!(
        dir.join("cache")
            .join(project_key(None))
            .join("lemma.fmt")
            .is_file()
    );
    assert_eq!(
        o.preamble_hash,
        lemma_preview::preamble_hash(DEFAULT_PREAMBLE)
    );
}

#[test]
fn colour_is_kept_and_environments_outside_math_render() {
    if !tex_available() {
        return;
    }
    let dir = temp("colour");
    let root = dir.join("paper.tex");
    fs::write(
        &root,
        "\\documentclass{article}\n\\usepackage{xcolor}\n\\usepackage{tikz-cd}\n\\begin{document}\n\
Red: $\\color{red} \\sum_{i=1}^n i^2$.\n\\begin{tikzcd}\nA \\arrow[r, \"f\"] & B\n\\end{tikzcd}\n\
\\end{document}\n",
    )
    .unwrap();
    let previewer = Previewer::new(dir.join("cache"));
    let o = previewer.render(&job(&root, &root, "\\sum"));
    let img = image(&o);
    snapshot("colour", img);
    assert!(!img.is_mask(), "red stays red");
    let j = job(&root, &root, "A \\arrow");
    assert_eq!(j.kind, FormulaKind::Environment("tikzcd".into()));
    let o = previewer.render(&j);
    let img = image(&o);
    snapshot("tikzcd-text", img);
    assert!(img.is_mask() && img.width > 80);
}

#[test]
fn a_missing_engine_is_reported() {
    let dir = temp("missing");
    let mut previewer = Previewer::new(dir.join("cache"));
    previewer.pdflatex = "lemma-no-such-tex".into();
    let o = previewer.render(&Job {
        root: None,
        dir: dir.clone(),
        preamble: PreambleSource::Default,
        definitions: String::new(),
        kind: FormulaKind::Inline,
        formula: "$x$".into(),
        scale: 1.0,
    });
    match o.result {
        Err(PreviewError::Missing(what)) => assert!(what.contains("lemma-no-such-tex"), "{what}"),
        other => panic!("{other:?}"),
    }
}

/// p50 ≤ 250 ms from request to pixels with a built format (ARCHITECTURE §5: 90–115 ms for
/// the TeX run alone), over distinct formulas of the fixture (no cache).
#[test]
#[ignore]
fn perf_render_latency() {
    if !tex_available() {
        return;
    }
    let (proj, cache) = project("perf");
    let main = proj.join("main.tex");
    let previewer = Previewer::new(&cache);
    image(&previewer.render(&job(&main, &main, "\\cO_X")));
    let mut times = Vec::new();
    let mut tex = Vec::new();
    let mut raster = Vec::new();
    for i in 0..30 {
        let mut j = job(&main, &main, "\\mathrm{QCoh}(S)\\longrightarrow");
        j.formula = j.formula.replacen("\\qquad", &format!("\\qquad {i}+"), 1);
        let started = Instant::now();
        let o = previewer.render(&j);
        times.push(started.elapsed());
        image(&o);
        tex.push(o.tex_time);
        raster.push(o.raster_time);
    }
    let median = |v: &mut Vec<Duration>| {
        v.sort();
        v[v.len() / 2]
    };
    let p50 = median(&mut times);
    println!(
        "render latency: p50 {p50:?}, p90 {:?}, max {:?}; TeX p50 {:?}, raster p50 {:?}",
        times[times.len() * 9 / 10],
        times[times.len() - 1],
        median(&mut tex),
        median(&mut raster)
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(p50 <= Duration::from_millis(250), "p50 {p50:?} over 250 ms");
    }
}
