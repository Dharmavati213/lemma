//! Scanner highlighting: LaTeX buffers drop GtkSourceView's language
//! and get one tag per token kind styled from the scheme, the math tint and the no-spell-check
//! tag; tagging is lazy (only lines that were in the visible window ± one screen carry tags) and
//! incremental (changes retag what the scanner re-scanned); deleted text of review hunks gets the
//! same colours.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_highlight`.
//! `UPDATE_GOLDEN=1` rewrites the golden token-to-tag files in `tests/golden/highlight/`.
//! The keystroke budget (§7, ≤ 0.5 ms p99 on `large.tex`) runs in release:
//! `scripts/broadway.sh cargo test --release -p lemma-app --test gtk_highlight -- --ignored perf`.
//! `LEMMA_TEST_SEED` / `LEMMA_TEST_OPS` (default 200) change the random-edits test.

mod common;

use gtk::prelude::*;
use lemma_agent::EditorRequest;
use lemma_app::agent_edits;
use lemma_app::document::DocRef;
use lemma_app::editor::Editor;
use lemma_app::editor::highlight::{self, Highlighter, MATH_TAG, NO_SPELL_TAG};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::{LineIndex, Origin, ProjectRoot, TextEdit};
use lemma_syntax::{Context, LineStates, StrLines, Token, TokenKind};
use sourceview5::prelude::*;
use std::collections::BTreeMap;
use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let mut tests: Vec<common::Test> = vec![
        ("latex_buffers_switch_to_the_scanner", latex_buffers),
        ("golden_tokens_to_tags", golden_tokens_to_tags),
        ("tag_styles_follow_the_scheme", styles_follow_the_scheme),
        ("math_tint_follows_the_config", math_tint_config),
        (
            "no_spell_check_covers_exactly_the_expected_spans",
            no_spell_spans,
        ),
        (
            "only_lines_in_a_visited_window_are_tagged",
            only_visited_lines_are_tagged,
        ),
        (
            "regress_layout_validation_does_not_retag",
            validation_does_not_retag,
        ),
        (
            "lines_moved_into_view_by_layout_are_tagged",
            lines_moved_into_view,
        ),
        ("edits_retag_the_converged_range", edits_retag),
        ("random_edits_match_a_fresh_scan", random_edits),
        (
            "lone_carriage_returns_are_tagged_exactly",
            lone_carriage_returns,
        ),
        (
            "large_documents_are_coloured_while_their_states_build",
            large_document_before_states,
        ),
        ("phantom_text_has_syntax_colours", phantom_text),
        ("attrs_for_colours_text_from_a_context", attrs_for_text),
        ("detach_disconnects_everything", detach),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_highlight_keystroke_large_tex", perf_keystroke));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(30);

const KINDS: [TokenKind; 20] = [
    TokenKind::Text,
    TokenKind::Command,
    TokenKind::Sectioning,
    TokenKind::Begin,
    TokenKind::End,
    TokenKind::EnvName,
    TokenKind::Brace,
    TokenKind::Bracket,
    TokenKind::Special,
    TokenKind::MathDelim,
    TokenKind::Math,
    TokenKind::MathCommand,
    TokenKind::Comment,
    TokenKind::Verbatim,
    TokenKind::LabelArg,
    TokenKind::RefArg,
    TokenKind::CiteArg,
    TokenKind::FileArg,
    TokenKind::PackageArg,
    TokenKind::KeyArg,
];

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
    }
}

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    fixture_with(name, files, |_| {})
}

fn fixture_with(
    name: &str,
    files: &[(&str, &str)],
    configure: impl FnOnce(&mut Config),
) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        // Until the view has its size and the scanner states exist.
        common::pump_until(TIMEOUT, || {
            editor.view().height() > 0 && editor.document().has_line_states()
        });
        if let Some(h) = highlight::highlighter(&editor) {
            h.flush();
        }
        editor
    }
}

fn hl(editor: &Editor) -> Rc<Highlighter> {
    highlight::highlighter(editor).expect("the editor is highlighted")
}

fn fixture_text(rel: &str) -> String {
    std::fs::read_to_string(common::fixtures().join(rel)).unwrap()
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/highlight")
}

// ----- tag spans: from the buffer, and expected from a fresh scan -----

/// One tagged span: `\n` line, byte range within the line, short tag name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    line: u32,
    start: u32,
    end: u32,
    tag: &'static str,
}

/// Our tags, by full name and short name.
fn our_tags() -> Vec<(&'static str, &'static str)> {
    let mut tags: Vec<(&str, &str)> = KINDS
        .iter()
        .filter_map(|&k| highlight::tag_name(k))
        .map(|name| (name, name.trim_start_matches("lemma-syntax-")))
        .collect();
    tags.push((MATH_TAG, "math"));
    tags.push((NO_SPELL_TAG, "no-spell"));
    tags
}

/// Every range of our tags in the editor's buffer, cut at line ends (a stale line may carry a
/// tag across its `\n`: text typed inside a tagged range takes the tag).
fn buffer_spans(editor: &Editor) -> Vec<Span> {
    let buffer = editor.buffer();
    let doc = editor.document();
    let table = buffer.tag_table();
    let pos = |iter: &gtk::TextIter| {
        let offset = doc.byte_offset(iter);
        doc.line_index().position(offset)
    };
    let mut spans = Vec::new();
    for (name, short) in our_tags() {
        let tag = table.lookup(name).expect("tag exists");
        let mut push = |from: &gtk::TextIter, to: &gtk::TextIter| {
            let ((l0, c0), (l1, c1)) = (pos(from), pos(to));
            for line in l0..=l1 {
                let start = if line == l0 { c0 } else { 0 };
                let end = if line == l1 {
                    c1
                } else {
                    doc.line_index().line_range(line).len() as u32
                };
                if start < end {
                    spans.push(Span {
                        line,
                        start,
                        end,
                        tag: short,
                    });
                }
            }
        };
        let mut iter = buffer.start_iter();
        let mut begin = iter.starts_tag(Some(&tag)).then_some(iter);
        while iter.forward_to_tag_toggle(Some(&tag)) {
            if iter.starts_tag(Some(&tag)) {
                begin = Some(iter);
            } else if let Some(b) = begin.take() {
                push(&b, &iter);
            }
        }
        if let Some(b) = begin {
            push(&b, &buffer.end_iter());
        }
    }
    spans.sort();
    spans
}

fn is_math(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Math | TokenKind::MathCommand | TokenKind::MathDelim
    )
}

/// The spans a fresh scan of `text` gives line `line` (the oracle): one per token with a tag,
/// maximal runs of no-spell tokens, and math runs (continued across a `&` before math, at the
/// line start or after math).
fn expected_line(states: &LineStates, line: u32, text: &str, out: &mut Vec<Span>) {
    let mut tokens: Vec<Token> = Vec::new();
    states.tokens(line, text, &mut tokens);
    let names: BTreeMap<&str, &str> = our_tags().into_iter().collect();
    let push_run = |runs: &mut Vec<Span>, t: &Token, tag: &'static str| match runs
        .iter_mut()
        .rev()
        .find(|s| s.tag == tag)
    {
        Some(last) if last.end == t.start && last.line == line => last.end = t.end,
        _ => runs.push(Span {
            line,
            start: t.start,
            end: t.end,
            tag,
        }),
    };
    let mut runs = Vec::new();
    let mut in_math = false;
    for (i, t) in tokens.iter().enumerate() {
        if let Some(name) = highlight::tag_name(t.kind) {
            out.push(Span {
                line,
                start: t.start,
                end: t.end,
                tag: names[name],
            });
        }
        if t.kind.no_spell() {
            push_run(&mut runs, t, "no-spell");
        }
        let next_math = tokens.get(i + 1).is_some_and(|n| is_math(n.kind));
        in_math =
            is_math(t.kind) || (t.kind == TokenKind::Special && next_math && (i == 0 || in_math));
        if in_math {
            push_run(&mut runs, t, "math");
        }
    }
    out.extend(runs);
}

/// Fresh scanner states of the document's text, with its rules.
fn fresh_states(doc: &DocRef, text: &str) -> (LineStates, LineIndex) {
    let index = LineIndex::new(text);
    let mut states = LineStates::new(doc.scan_config());
    states.rebuild(&StrLines {
        text,
        index: &index,
    });
    (states, index)
}

fn expected_spans(doc: &DocRef, lines: Range<u32>) -> Vec<Span> {
    let text = doc.text();
    let (states, index) = fresh_states(doc, &text);
    let mut out = Vec::new();
    for line in lines.start..lines.end.min(index.line_count()) {
        expected_line(&states, line, &text[index.line_range(line)], &mut out);
    }
    out.sort();
    out
}

fn spans_in(spans: &[Span], lines: &Range<u32>) -> Vec<Span> {
    spans
        .iter()
        .filter(|s| lines.contains(&s.line))
        .cloned()
        .collect()
}

/// The golden text: one span per line, with the text it covers.
fn render(spans: &[Span], text: &str) -> String {
    let index = LineIndex::new(text);
    let mut out = String::new();
    for s in spans {
        let line = &text[index.line_range(s.line)];
        out.push_str(&format!(
            "{}:{}-{} {} {:?}\n",
            s.line + 1,
            s.start,
            s.end,
            s.tag,
            &line[s.start as usize..s.end as usize]
        ));
    }
    out
}

// ----- tests -----

fn latex_buffers() {
    let f = fixture(
        "language",
        &[
            ("main.tex", "\\section{A} (x) [y] {z}\n$x$\n"),
            ("pkg.sty", "\\def\\x{1}\n"),
            ("refs.bib", "@book{a, title={T}}\n"),
            ("notes.txt", "plain\n"),
        ],
    );
    let tex = f.open("main.tex");
    assert!(
        tex.buffer().language().is_none(),
        "no GtkSourceView language"
    );
    // Brackets are matched by `editor::pairs` (WP3.9), which knows comments, escapes and `\left`.
    assert!(!tex.buffer().is_highlight_matching_brackets());
    let table = tex.buffer().tag_table();
    for (name, _) in our_tags() {
        assert!(table.lookup(name).is_some(), "tag {name}");
    }
    let mut names = Vec::new();
    table.foreach(|tag| {
        if let Some(name) = tag.name() {
            names.push(name.to_string());
        }
    });
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("latex:")
                || (n.starts_with("gtksourceview:") && n != NO_SPELL_TAG)),
        "GtkSourceView's own highlighting left tags: {names:?}"
    );
    // The visible lines are tagged once the view is shown.
    assert!(hl(&tex).is_current(0) && hl(&tex).is_current(1));
    assert_eq!(
        buffer_spans(&tex),
        expected_spans(tex.document(), 0..3),
        "the first lines carry their tokens' tags"
    );
    // Other LaTeX files too; BibTeX and plain text keep their language.
    let sty = f.open("pkg.sty");
    assert!(highlight::highlighter(&sty).is_some());
    let bib = f.open("refs.bib");
    assert_eq!(
        bib.buffer()
            .language()
            .map(|l| l.id().to_string())
            .as_deref(),
        Some("bibtex")
    );
    assert!(highlight::highlighter(&bib).is_none());
    let txt = f.open("notes.txt");
    assert!(highlight::highlighter(&txt).is_none());
    // Attaching twice does nothing.
    highlight::attach(&f.state, &tex);
    assert_eq!(buffer_spans(&tex), expected_spans(tex.document(), 0..3));
}

fn golden_tokens_to_tags() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let cases = [
        ("main", fixture_text("proj/main.tex")),
        ("yoneda", fixture_text("proj/chapters/yoneda.tex")),
        (
            "syntax",
            std::fs::read_to_string(golden_dir().join("syntax.tex")).unwrap(),
        ),
    ];
    let mut failures = Vec::new();
    for (name, text) in cases {
        let file = format!("{name}.tex");
        let f = fixture(&format!("golden-{name}"), &[(&file, &text)]);
        let ed = f.open(&file);
        let h = hl(&ed);
        h.tag_all();
        let count = ed.document().line_index().line_count();
        assert!((0..count).all(|l| h.is_current(l)));
        let spans = buffer_spans(&ed);
        // The buffer carries exactly the tags of a fresh scan …
        assert_eq!(spans, expected_spans(ed.document(), 0..count), "{name}");
        // … and the scan's mapping to tags is the recorded one.
        let got = render(&spans, &text);
        let path = golden_dir().join(format!("{name}.golden"));
        if update {
            std::fs::write(&path, &got).unwrap();
            continue;
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing {} (run with UPDATE_GOLDEN=1)", path.display()));
        if got != want {
            let first = got
                .lines()
                .zip(want.lines())
                .find(|(g, w)| g != w)
                .map(|(g, w)| format!("got  {g}\nwant {w}"))
                .unwrap_or_else(|| "lengths differ".into());
            failures.push(format!("{}: first difference:\n{first}", path.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

const SAMPLE: &str = "\\section{Intro}\\label{s}\n\
Let $x \\in X$ by \\cite{K}. % note\n\
\\begin{align}\n  a &= b \\\\\n\\end{align}\n\
\\input{file} \\verb|v| x\\%y~z\n";

fn rgba(spec: &str) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::parse(spec).unwrap()
}

/// What the scheme says `kind` looks like: foreground and whether it is bold.
fn scheme_look(
    scheme: &sourceview5::StyleScheme,
    kind: TokenKind,
) -> (Option<gtk::gdk::RGBA>, Option<bool>) {
    let style = highlight::scheme_style(Some(scheme), kind);
    let fg = style
        .as_ref()
        .filter(|s| s.is_foreground_set())
        .and_then(|s| s.foreground())
        .map(|c| rgba(&c));
    let bold = style
        .as_ref()
        .filter(|s| s.is_bold_set())
        .map(|s| s.is_bold());
    (fg, bold)
}

fn styles_follow_the_scheme() {
    let f = fixture("styles", &[("main.tex", SAMPLE)]);
    let ed = f.open("main.tex");
    let h = hl(&ed);
    let tagged = h.tagged_lines();
    let manager = sourceview5::StyleSchemeManager::default();
    for id in [
        "Adwaita",
        "Adwaita-dark",
        "solarized-light",
        "kate-dark",
        "classic",
    ] {
        let scheme = manager.scheme(id).expect("scheme installed");
        ed.set_style_scheme(Some(&scheme));
        for kind in KINDS {
            let Some(tag) = h.tag(kind) else {
                assert!(highlight::tag_name(kind).is_none());
                continue;
            };
            let (fg, bold) = scheme_look(&scheme, kind);
            let tag_fg = tag
                .is_foreground_set()
                .then(|| tag.foreground_rgba())
                .flatten();
            assert_eq!(tag_fg, fg, "{id}: foreground of {kind:?}");
            match bold {
                Some(true) => assert_eq!(tag.weight(), 700, "{id}: {kind:?} is bold"),
                Some(false) => assert_eq!(tag.weight(), 400, "{id}: {kind:?} is normal"),
                None => assert!(!tag.is_weight_set(), "{id}: {kind:?} keeps the weight"),
            }
        }
        // The math tag has math's colour, and its tint is that colour at `editor.math_tint` (6)
        // percent.
        let math = highlight::math_color(Some(&scheme)).expect("math colour");
        assert_eq!(h.math_tag().foreground_rgba(), Some(math), "{id}");
        assert_eq!(scheme_look(&scheme, TokenKind::Math).0, Some(math), "{id}");
        let tint = h.math_tag().background_rgba().expect("tint");
        assert_eq!(
            (tint.red(), tint.green(), tint.blue()),
            (math.red(), math.green(), math.blue()),
            "{id}"
        );
        assert!((tint.alpha() - 0.06 * math.alpha()).abs() < 1e-4, "{id}");
        // Restyling moves no tag.
        assert_eq!(h.tagged_lines(), tagged);
        assert_eq!(buffer_spans(&ed), expected_spans(ed.document(), 0..8));
    }
    // The scheme decides: commands follow `def:statement` in Adwaita-dark (no `def:keyword`),
    // math `def:constant` (no `def:special-constant`), environment names `def:type`.
    let dark = manager.scheme("Adwaita-dark").unwrap();
    ed.set_style_scheme(Some(&dark));
    let command = h.tag(TokenKind::Command).unwrap();
    assert_eq!(command.foreground_rgba(), Some(rgba("#FFA348")));
    assert_eq!(command.weight(), 700);
    let math = h.math_tag();
    assert_eq!(math.foreground_rgba(), Some(rgba("#7D8AC7")));
    assert!(!math.is_weight_set());
    let env = h.tag(TokenKind::EnvName).unwrap();
    assert_eq!(env.foreground_rgba(), Some(rgba("#5BC8AF")));
    // In cobalt, `def:special-constant` exists and is bold; math commands, shown over the math
    // tag, stay normal (their `def:function` is not bold).
    ed.set_style_scheme(manager.scheme("cobalt").as_ref());
    assert_eq!(math.foreground_rgba(), Some(rgba("#ff0044")));
    assert_eq!(math.weight(), 700);
    let math_command = h.tag(TokenKind::MathCommand).unwrap();
    assert_eq!(math_command.foreground_rgba(), Some(rgba("#cccccc")));
    assert_eq!(math_command.weight(), 400);
    // No scheme: no styles at all; the tint takes the text colour.
    ed.set_style_scheme(None);
    assert!(!command.is_foreground_set() && !command.is_weight_set());
    assert!(!math.is_foreground_set() && math.is_background_set());
}

fn math_tint_config() {
    let f = fixture_with("tint", &[("main.tex", SAMPLE)], |c| c.editor.math_tint = 20);
    let ed = f.open("main.tex");
    let h = hl(&ed);
    let tint = h.math_tag();
    let alpha = |tag: &gtk::TextTag| tag.background_rgba().map(|c| c.alpha());
    let math = highlight::math_color(ed.buffer().style_scheme().as_ref()).unwrap();
    assert!((alpha(&tint).unwrap() - 0.2 * math.alpha()).abs() < 1e-4);
    // A config reload applies at once, without retagging.
    let spans = buffer_spans(&ed);
    let mut config = (*f.state.config.get()).clone();
    config.editor.math_tint = 0;
    f.state.set_config(Rc::new(config.clone()));
    assert!(
        !tint.is_background_set(),
        "math_tint = 0 turns the tint off"
    );
    config.editor.math_tint = 100;
    f.state.set_config(Rc::new(config));
    assert!((alpha(&tint).unwrap() - math.alpha()).abs() < 1e-4);
    assert_eq!(buffer_spans(&ed), spans);
    // The tint covers the math runs, including the `&` of an align row.
    let tint_text: Vec<String> = spans
        .iter()
        .filter(|s| s.tag == "math")
        .map(|s| {
            let line = ed.document().line_text(s.line).unwrap();
            line[s.start as usize..s.end as usize].to_owned()
        })
        .collect();
    assert_eq!(tint_text, ["$x \\in X$", "  a &= b \\\\"]);
}

fn no_spell_spans() {
    let text = "Proof: see \\cite{Key}, \\ref{thm:a} and \\label{lab}; \\emph{checked words} $x+y$.\n\
% comment words\n\
\\begin{theorem}[Named result] Body text \\textbf{bold words}.\\end{theorem}\n\
\\usepackage[opt]{pkg} \\input{file/name} \\includegraphics{fig.pdf} \\bibliography{refs}\n\
Math \\(a\\) and $\\text{inner text}$ end.\n\
\\verb|xx yy| tail \\caption{A caption} \\footnote{A note}\n\
\\newtheorem{lem}[thm]{Lemma} \\hyperref[sec:a]{the text} \\crefname{lem}{lemma}{lemmas}\n\
\\DeclareMathOperator{\\coker}{coker} A \\arrow[rr, \"f\"] \\draw[thick] (0,0);\n";
    // The text each no-spell span covers, line by line, in order.
    let expected: [&[&str]; 8] = [
        &[
            "\\cite{Key}",
            "\\ref{thm:a}",
            "\\label{lab}",
            "\\emph{",
            "}",
            "$x+y$",
        ],
        &["% comment words"],
        &["\\begin{theorem}[", "]", "\\textbf{", "}", "\\end{theorem}"],
        &[
            "\\usepackage[opt]{pkg}",
            "\\input{file/name}",
            "\\includegraphics{fig.pdf}",
            "\\bibliography{refs}",
        ],
        &["\\(a\\)", "$\\text{", "}$"],
        &["\\verb|xx yy|", "\\caption{", "}", "\\footnote{", "}"],
        // Theorem counters, hyperref labels, cleveref counter names, operator names and TikZ
        // options (E1-4).
        &[
            "\\newtheorem{lem}[thm]{",
            "}",
            "\\hyperref[sec:a]{",
            "}",
            "\\crefname{lem}{",
            "}{",
            "}",
        ],
        &[
            "\\DeclareMathOperator{\\coker}{coker}",
            "\\arrow[rr, \"f\"]",
            "\\draw[thick]",
        ],
    ];
    let f = fixture("no-spell", &[("main.tex", text)]);
    let ed = f.open("main.tex");
    let doc = ed.document();
    let got: Vec<Vec<String>> = (0..8)
        .map(|line| {
            let line_text = doc.line_text(line).unwrap();
            buffer_spans(&ed)
                .iter()
                .filter(|s| s.tag == "no-spell" && s.line == line)
                .map(|s| line_text[s.start as usize..s.end as usize].to_owned())
                .collect()
        })
        .collect();
    let want: Vec<Vec<String>> = expected
        .iter()
        .map(|l| l.iter().map(|s| s.to_string()).collect())
        .collect();
    assert_eq!(got, want);
    // The tag is the one libspelling looks up, and checked text has none of it.
    let buffer = ed.buffer();
    let tag = buffer.tag_table().lookup(NO_SPELL_TAG).unwrap();
    assert_eq!(tag, hl(&ed).no_spell_tag());
    let at = |line: i32, col: i32| buffer.iter_at_line_offset(line, col).unwrap();
    assert!(!at(0, 0).has_tag(&tag), "\"Proof\" is checked");
    assert!(!at(0, 59).has_tag(&tag), "\\emph text is checked");
    assert!(at(0, 17).has_tag(&tag), "citation keys are not");
}

/// Lines `(first, last)` the test sees in the view (independently of the highlighter).
fn visible_lines(ed: &Editor) -> (u32, u32) {
    let view = ed.view();
    let rect = view.visible_rect();
    let doc = ed.document();
    let line = |y: i32| {
        let (iter, _) = view.line_at_y(y);
        doc.line_index().line_of(doc.byte_offset(&iter))
    };
    (line(rect.y()), line(rect.y() + rect.height() - 1))
}

fn union(ranges: &[Range<u32>]) -> Vec<Range<u32>> {
    let mut sorted = ranges.to_vec();
    sorted.sort_by_key(|r| r.start);
    let mut out: Vec<Range<u32>> = Vec::new();
    for r in sorted {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

fn only_visited_lines_are_tagged() {
    let f = fixture("lazy", &[("large.tex", &fixture_text("large.tex"))]);
    let ed = f.open("large.tex");
    let h = hl(&ed);
    let doc = ed.document();
    let count = doc.line_index().line_count();
    assert_eq!(count, 60_004);
    let window = h.window().expect("a window");
    let mut visited = vec![window.tagged.clone()];
    assert_eq!(h.tagged_lines(), visited, "only the first window is tagged");
    assert!(window.tagged.end < 200, "{window:?}");
    // Jump around the first third of the document.
    let adjustment = ed.view().vadjustment().unwrap();
    for k in [3.0, 1.0, 7.5, 7.6, 12.0, 2.0, 30.0] {
        adjustment.set_value(adjustment.upper() * k / 100.0);
        h.flush();
        let window = h.window().unwrap();
        let (first, last) = visible_lines(&ed);
        assert!(
            window.visible.start <= first && last < window.visible.end,
            "the window {window:?} covers the visible lines {first}..={last}"
        );
        let screen = window.visible.len() as u32;
        assert_eq!(
            window.tagged.start,
            window.visible.start.saturating_sub(screen)
        );
        assert_eq!(window.tagged.end, (window.visible.end + screen).min(count));
        visited.push(window.tagged.clone());
        assert_eq!(h.tagged_lines(), union(&visited));
    }
    // Nothing outside them carries a tag, and what they carry is exact.
    let spans = buffer_spans(&ed);
    for s in &spans {
        assert!(
            h.is_tagged(s.line),
            "a tag on line {} outside every window",
            s.line + 1
        );
    }
    for r in union(&visited) {
        assert_eq!(
            spans_in(&spans, &r),
            expected_spans(doc, r.clone()),
            "{r:?}"
        );
    }
    // Scrolling through everything tags everything (and nothing twice: the work is per line).
    let mut value = 0.0;
    loop {
        adjustment.set_value(value);
        h.flush();
        if value >= adjustment.upper() - adjustment.page_size() {
            break;
        }
        value =
            (value + adjustment.page_size() * 1.5).min(adjustment.upper() - adjustment.page_size());
    }
    assert_eq!(h.tagged_lines(), vec![0..count]);
    assert!((0..count).all(|l| h.is_current(l)));
}

/// GTK validates a long text's layout in the background, and every step grows the scroll range
/// (`Adjustment::changed`). Those steps must not each start a tagging pass: on a 180k-line text
/// that starved the main loop (a `:wq` regression test timed out opening the file).
fn validation_does_not_retag() {
    let large = fixture_text("large.tex");
    let big = [large.as_str(), &large, &large].concat();
    let f = fixture("validation", &[("big.tex", &big)]);
    // Count the range changes from the moment the tab exists.
    let changes = Rc::new(std::cell::Cell::new(0u32));
    let seen = changes.clone();
    f.state.tab_view.connect_page_attached(move |_, page, _| {
        if let Ok(scroller) = page.child().downcast::<gtk::ScrolledWindow>() {
            let seen = seen.clone();
            scroller
                .vadjustment()
                .connect_changed(move |_| seen.set(seen.get() + 1));
        }
    });
    let editor = block_on(f.state.open_file(&f.dir.join("big.tex"))).unwrap();
    let h = hl(&editor);
    let adjustment = editor.view().vadjustment().unwrap();
    // Until GTK has laid out the whole text (the range stops growing).
    let mut last = adjustment.upper();
    let mut quiet = 0;
    common::pump_until(Duration::from_secs(120), || {
        common::pump_for(Duration::from_millis(50));
        if adjustment.upper() == last {
            quiet += 1;
        } else {
            quiet = 0;
            last = adjustment.upper();
        }
        quiet >= 10 && editor.document().has_line_states()
    });
    let passes = h.passes();
    println!(
        "    {} range changes while GTK laid the text out; {passes} tagging passes",
        changes.get()
    );
    assert!(changes.get() > 20, "GTK validated the layout in steps");
    assert!(
        passes <= 10,
        "{passes} tagging passes for {} range changes",
        changes.get()
    );
    h.flush();
    window_matches(&editor, &h);
}

/// Lines can come into view without a scroll or an edit: a tall band above them (a review
/// hunk's deleted text, reading mode) shrinks. They are tagged once the main loop is idle.
fn lines_moved_into_view() {
    let text: String = (0..600)
        .map(|i| format!("Line {i} with \\emph{{x}} and $y_{i}$.\n"))
        .collect();
    let f = fixture("moved", &[("main.tex", &text)]);
    let ed = f.open("main.tex");
    let h = hl(&ed);
    let buffer = ed.buffer();
    // A 6000-pixel band above line 200, as a review hunk reserves, far from the tagged top.
    let band = gtk::TextTag::new(None);
    buffer.tag_table().add(&band);
    band.set_pixels_above_lines(6000);
    let start = buffer.iter_at_line(200).unwrap();
    let mut end = start;
    end.forward_to_line_end();
    buffer.apply_tag(&band, &start, &end);
    common::pump_for(Duration::from_millis(200));
    // Line 199 at the top: the band fills the rest of the screen.
    let adjustment = ed.view().vadjustment().unwrap();
    let (y, _) = ed.view().line_yrange(&buffer.iter_at_line(199).unwrap());
    adjustment.set_value(f64::from(y));
    h.flush();
    let before = h.window().unwrap();
    assert!(before.visible.end <= 202, "{before:?}");
    // The band goes (the hunk was decided): the lines below it come up into view, without a
    // scroll or an edit.
    let value = adjustment.value();
    band.set_pixels_above_lines(0);
    common::pump_until(TIMEOUT, || {
        let (first, last) = visible_lines(&ed);
        last >= before.tagged.end && (first..=last).all(|l| h.is_current(l))
    });
    assert_eq!(adjustment.value(), value, "no scroll happened");
    h.flush();
    window_matches(&ed, &h);
}

/// Type `text` at `offset` as the user (one insertion per character, like keystrokes).
fn type_at(ed: &Editor, offset: usize, text: &str) {
    let doc = ed.document();
    let mut at = offset;
    for c in text.chars() {
        let mut iter = doc.iter_at_byte(at).unwrap();
        ed.buffer().insert(&mut iter, &c.to_string());
        at += c.len_utf8();
    }
}

fn window_matches(ed: &Editor, h: &Highlighter) {
    let window = h.window().unwrap();
    let spans = buffer_spans(ed);
    for line in window.tagged.clone() {
        assert!(
            h.is_current(line),
            "line {} of the window is current",
            line + 1
        );
    }
    assert_eq!(
        spans_in(&spans, &window.tagged),
        expected_spans(ed.document(), window.tagged.clone()),
        "window {window:?}"
    );
    for s in &spans {
        assert!(h.is_tagged(s.line), "line {} carries tags", s.line + 1);
    }
}

fn edits_retag() {
    // 400 lines: a paragraph at the top, the rest plain lines.
    let mut text = String::from("\\section{Top}\nAlpha beta gamma\ndelta epsilon\nzeta eta\n\n");
    for i in 0..395 {
        text.push_str(&format!("Line {i} with \\emph{{x}} and $y_{i}$.\n"));
    }
    let f = fixture("edits", &[("main.tex", &text)]);
    let ed = f.open("main.tex");
    let h = hl(&ed);
    let doc = ed.document().clone();
    window_matches(&ed, &h);
    // An unmatched `$` makes the rest of the paragraph math; the visible lines follow at once.
    let offset = doc.line_index().line_start(1);
    type_at(&ed, offset, "$");
    h.flush();
    window_matches(&ed, &h);
    let math = h.math_tag();
    let line_iter = |line: i32| ed.buffer().iter_at_line(line).unwrap();
    assert!(
        line_iter(3).has_tag(&math),
        "the paragraph's last line is math"
    );
    assert!(!line_iter(5).has_tag(&math), "the blank line ended it");
    // Closing it on the same line restores the text.
    type_at(&ed, offset + 6, "$");
    h.flush();
    window_matches(&ed, &h);
    assert!(!line_iter(3).has_tag(&math));

    // Visit the end, then open an environment at the top that stays open to the end: the
    // visible lines (at the end) are retagged now, the tagged lines at the top become stale,
    // and the lines never visited stay untagged.
    let adjustment = ed.view().vadjustment().unwrap();
    adjustment.set_value(adjustment.upper());
    h.flush();
    let top = h.window().unwrap();
    let before = h.tagged_lines();
    assert_eq!(
        before.len(),
        2,
        "the top and the end are tagged: {before:?}"
    );
    let at = doc.line_index().line_start(5);
    doc.apply_edits(
        &[TextEdit::insert(at, "\\begin{equation}\n")],
        Origin::Agent { call: 1 },
    )
    .unwrap();
    h.flush();
    window_matches(&ed, &h);
    let count = doc.line_index().line_count();
    let last = count - 2;
    assert!(line_iter(last as i32).has_tag(&math), "the end is math now");
    assert!(h.is_current(1), "lines before the change keep their tags");
    assert!(
        !h.is_current(10) && h.is_tagged(10),
        "tagged lines after it are stale"
    );
    assert!(!h.is_tagged(200), "a line never visited stays untagged");
    assert!(top.tagged.end > 0);
    // Back at the top: the stale lines are retagged.
    adjustment.set_value(0.0);
    h.flush();
    window_matches(&ed, &h);
    // Undo takes the environment back out.
    ed.buffer().undo();
    h.flush();
    window_matches(&ed, &h);
    assert!(!line_iter(10).has_tag(&math));
    // Deleting a tagged line and joining lines leave no stray tags.
    let (a, b) = (
        doc.line_index().line_start(7),
        doc.line_index().line_start(9),
    );
    doc.apply_edits(&[TextEdit::delete(a - 1..b + 3)], Origin::User)
        .unwrap();
    h.flush();
    window_matches(&ed, &h);
    // Enter inside a tagged command: the new line takes no stale tag.
    let at = doc.line_index().line_start(0) + 4;
    type_at(&ed, at, "\n");
    h.flush();
    window_matches(&ed, &h);
}

fn random_edits() {
    let seed: u64 = std::env::var("LEMMA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_2707);
    let ops: usize = std::env::var("LEMMA_TEST_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    println!("    seed {seed}, {ops} operations");
    let f = fixture("random", &[("main.tex", &fixture_text("proj/main.tex"))]);
    let ed = f.open("main.tex");
    let h = hl(&ed);
    let doc = ed.document().clone();
    let buffer = ed.buffer().clone();
    let adjustment = ed.view().vadjustment().unwrap();
    let mut rng = common::Rng::new(seed);
    const PIECES: [&str; 18] = [
        "$",
        "\\[",
        "\\]",
        "{",
        "}",
        "%",
        "\\begin{align}\n",
        "\\end{align}\n",
        "\\section{x}",
        "\n",
        "\n\n",
        "word ",
        "\\cite{a,",
        "&",
        "\\verb|",
        "|",
        "\\begin{verbatim}\n",
        "\\end{verbatim}",
    ];
    for op in 0..ops {
        let len = doc.line_index().text_len();
        let pick = |rng: &mut common::Rng| {
            let text = doc.text();
            let mut at = rng.below(len as u64 + 1) as usize;
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            at
        };
        match rng.below(10) {
            0..=4 => {
                let at = pick(&mut rng);
                let piece = PIECES[rng.below(PIECES.len() as u64) as usize];
                if rng.below(2) == 0 {
                    type_at(&ed, at, piece);
                } else {
                    doc.apply_edits(&[TextEdit::insert(at, piece)], Origin::Agent { call: 1 })
                        .unwrap();
                }
            }
            5 | 6 => {
                let a = pick(&mut rng);
                let text = doc.text();
                let mut b = (a + rng.below(80) as usize).min(text.len());
                while !text.is_char_boundary(b) {
                    b -= 1;
                }
                doc.apply_edits(&[TextEdit::delete(a..b)], Origin::User)
                    .unwrap();
            }
            7 => {
                if buffer.can_undo() {
                    buffer.undo();
                }
            }
            8 => {
                if buffer.can_redo() {
                    buffer.redo();
                }
            }
            _ => {
                let fraction = rng.below(100) as f64 / 100.0;
                adjustment.set_value((adjustment.upper() - adjustment.page_size()) * fraction);
            }
        }
        h.flush();
        // Lines marked current carry exactly their tokens' tags, untagged lines none.
        let spans = buffer_spans(&ed);
        let count = doc.line_index().line_count();
        let current: Vec<Range<u32>> = (0..count)
            .filter(|&l| h.is_current(l))
            .map(|l| l..l + 1)
            .collect();
        let expected = expected_spans(&doc, 0..count);
        for r in union(&current) {
            assert_eq!(
                spans_in(&spans, &r),
                spans_in(&expected, &r),
                "operation {op}: lines {r:?} (seed {seed})"
            );
        }
        for s in &spans {
            assert!(
                h.is_tagged(s.line),
                "operation {op}: tags on untagged line {}",
                s.line
            );
        }
        let window = h.window().unwrap();
        assert!(
            window.tagged.clone().all(|l| h.is_current(l)),
            "operation {op}: the window {window:?} is current"
        );
    }
}

/// GTK also breaks lines at a lone `\r` (and U+2029), `LineIndex` only at `\n`: positions are
/// then found by counting characters. Tags still land exactly, also after edits.
fn lone_carriage_returns() {
    let text = "a\rb $x$ and \\cite{k}\r\n\\section{T}\rmore $y\u{2029}z$ \\emph{e}\n% c\r$$\n";
    let f = fixture("lone-cr", &[("main.tex", text)]);
    let ed = f.open("main.tex");
    let h = hl(&ed);
    let doc = ed.document().clone();
    assert_ne!(
        ed.buffer().line_count() as u32,
        doc.line_index().line_count(),
        "GTK counts more lines"
    );
    let count = doc.line_index().line_count();
    assert!((0..count).all(|l| h.is_current(l)));
    assert_eq!(buffer_spans(&ed), expected_spans(&doc, 0..count));
    // Typing (math opened and closed) and an agent edit keep them exact.
    type_at(&ed, 2, "$");
    h.flush();
    assert_eq!(buffer_spans(&ed), expected_spans(&doc, 0..count));
    let at = doc.line_index().line_start(1);
    doc.apply_edits(
        &[TextEdit::insert(at, "\\begin{equation}\r")],
        Origin::Agent { call: 1 },
    )
    .unwrap();
    h.flush();
    let count = doc.line_index().line_count();
    assert_eq!(buffer_spans(&ed), expected_spans(&doc, 0..count));
}

fn large_document_before_states() {
    let f = fixture(
        "before-states",
        &[("large.tex", &fixture_text("large.tex"))],
    );
    let editor = block_on(f.state.open_file(&f.dir.join("large.tex"))).unwrap();
    let doc = editor.document().clone();
    let h = hl(&editor);
    // Tags lines while the jobs thread builds the states: only the first lines can be (they are
    // scanned on their own), and exactly.
    let check_top = |what: &str| {
        assert!(!doc.has_line_states(), "{what}: the states are being built");
        h.flush();
        assert!(
            h.is_current(0),
            "{what}: the top is tagged while the states are built"
        );
        let spans = buffer_spans(&editor);
        for r in h.tagged_lines() {
            for line in r.clone() {
                if h.is_current(line) {
                    assert_eq!(
                        spans_in(&spans, &(line..line + 1)),
                        expected_spans(&doc, line..line + 1),
                        "{what}: line {}",
                        line + 1
                    );
                }
            }
        }
    };
    if doc.has_line_states() {
        println!("    the scan at open finished before the first check");
    } else {
        check_top("open");
    }
    common::pump_until(TIMEOUT, || {
        doc.has_line_states() && editor.view().height() > 0
    });
    h.flush();
    window_matches(&editor, &h);
    // New scanner rules rebuild the states on the jobs thread; typing meanwhile retags the top.
    let mut rules = (*doc.scan_config()).clone();
    rules.math_envs.push("mymath".into());
    doc.set_scan_config(std::sync::Arc::new(rules));
    let at = doc.line_index().line_start(8);
    type_at(&editor, at, "$");
    check_top("rebuild");
    // A window far from the top waits for the states.
    let adjustment = editor.view().vadjustment().unwrap();
    adjustment.set_value(adjustment.upper() / 2.0);
    h.flush();
    let window = h.window().unwrap();
    if !doc.has_line_states() {
        assert!(window.visible.clone().all(|l| !h.is_current(l)));
    }
    common::pump_until(TIMEOUT, || doc.has_line_states());
    h.flush();
    window_matches(&editor, &h);
}

fn phantom_text() {
    let text = "Intro.\n\\begin{align}\n  a &= \\alpha + b \\\\\n  c &= d\n\\end{align}\n\
% note here\nEnd.\n";
    let f = fixture("phantom", &[("main.tex", text)]);
    let ed = f.open("main.tex");
    let reply = |old: &str, new: &str| {
        block_on(agent_edits::apply(
            &f.state.workspace,
            1,
            EditorRequest::Edit {
                path: "main.tex".into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all: false,
            },
        ))
    };
    assert!(!reply("  a &= \\alpha + b \\\\\n", "  a &= \\beta \\\\\n").is_error);
    assert!(!reply("% note here\n", "").is_error);
    common::pump_until(TIMEOUT, || {
        ed.view()
            .review_state()
            .is_some_and(|r| r.blocks().len() == 2)
    });
    let review = ed.view().review_state().unwrap();
    let scheme = ed.buffer().style_scheme();
    let color = |kind: TokenKind| {
        highlight::scheme_style(scheme.as_ref(), kind)
            .and_then(|s| s.foreground())
            .map(|c| {
                let c = rgba(&c);
                let ch = |v: f32| (v * 65535.0).round() as u16;
                (ch(c.red()), ch(c.green()), ch(c.blue()))
            })
            .expect("a foreground")
    };
    let foreground_at = |layout: &pango::Layout, needle: &str| {
        let text = layout.text();
        let at = text.find(needle).expect("text in the layout") as u32;
        layout
            .attributes()
            .expect("attributes")
            .attributes()
            .into_iter()
            .filter(|a| a.start_index() <= at && at < a.end_index())
            .find_map(|a| {
                a.downcast_ref::<pango::AttrColor>()
                    .filter(|c| c.type_() == pango::AttrType::Foreground)
                    .map(|c| (c.color().red(), c.color().green(), c.color().blue()))
            })
    };
    let blocks = review.blocks();
    // The deleted math line starts inside `align`: its math has math's colours.
    let math_line = review.phantom_layouts(blocks[0].id);
    assert_eq!(math_line.len(), 1);
    assert_eq!(math_line[0].text().as_str(), "  a &= \\alpha + b \\\\");
    assert_eq!(
        foreground_at(&math_line[0], "\\alpha"),
        Some(color(TokenKind::MathCommand))
    );
    assert_eq!(
        foreground_at(&math_line[0], "a &"),
        Some(color(TokenKind::Math))
    );
    // The deleted comment is coloured as a comment.
    let comment = review.phantom_layouts(blocks[1].id);
    assert_eq!(
        foreground_at(&comment[0], "note"),
        Some(color(TokenKind::Comment))
    );
    // A scheme change lays the deleted text out again in the new colours.
    let manager = sourceview5::StyleSchemeManager::default();
    let other = if scheme.as_ref().map(|s| s.id()) == Some("kate".into()) {
        "classic"
    } else {
        "kate"
    };
    ed.set_style_scheme(manager.scheme(other).as_ref());
    common::pump_until(TIMEOUT, || {
        let review = ed.view().review_state().unwrap();
        let blocks = review.blocks();
        let comment = review.phantom_layouts(blocks[1].id);
        !comment.is_empty()
            && foreground_at(&comment[0], "note")
                == highlight::scheme_style(manager.scheme(other).as_ref(), TokenKind::Comment)
                    .and_then(|s| s.foreground())
                    .map(|c| {
                        let c = rgba(&c);
                        let ch = |v: f32| (v * 65535.0).round() as u16;
                        (ch(c.red()), ch(c.green()), ch(c.blue()))
                    })
    });
}

fn attrs_for_text() {
    let manager = sourceview5::StyleSchemeManager::default();
    let scheme = manager.scheme("Adwaita").unwrap();
    let text = "x^2 + \\frac{a}{b}$ then % c\n\\textbf{t}";
    let attrs = highlight::attrs_for(
        text,
        &Context::Math {
            display: false,
            env: None,
        },
        Some(&scheme),
    );
    let fg = |at: usize| {
        attrs
            .attributes()
            .into_iter()
            .filter(|a| a.start_index() <= at as u32 && (at as u32) < a.end_index())
            .find_map(|a| {
                a.downcast_ref::<pango::AttrColor>()
                    .filter(|c| c.type_() == pango::AttrType::Foreground)
                    .map(|c| c.color().to_str().to_string())
            })
    };
    let hex = |kind: TokenKind| {
        let c = rgba(
            &highlight::scheme_style(Some(&scheme), kind)
                .unwrap()
                .foreground()
                .unwrap(),
        );
        let ch = |v: f32| (v * 65535.0).round() as u16;
        pango::Color::parse(&format!(
            "#{:04x}{:04x}{:04x}",
            ch(c.red()),
            ch(c.green()),
            ch(c.blue())
        ))
        .unwrap()
        .to_str()
        .to_string()
    };
    assert_eq!(fg(0), Some(hex(TokenKind::Math)), "starts in math");
    assert_eq!(
        fg(text.find("\\frac").unwrap()),
        Some(hex(TokenKind::MathCommand))
    );
    assert_eq!(fg(text.find('$').unwrap()), Some(hex(TokenKind::MathDelim)));
    assert_eq!(
        fg(text.find("then").unwrap()),
        None,
        "text keeps the layout's colour"
    );
    assert_eq!(fg(text.find('%').unwrap()), Some(hex(TokenKind::Comment)));
    assert_eq!(
        fg(text.find("textbf").unwrap()),
        Some(hex(TokenKind::Command))
    );
    // Bold where the scheme says so, never a background (the deleted text keeps its tint).
    assert!(
        attrs
            .attributes()
            .iter()
            .any(|a| a.type_() == pango::AttrType::Weight)
    );
    assert!(
        attrs
            .attributes()
            .iter()
            .all(|a| a.type_() != pango::AttrType::Background)
    );
}

fn detach() {
    let f = fixture("detach", &[("main.tex", SAMPLE), ("b.tex", "b $x$\n")]);
    let ed = f.open("main.tex");
    let other = f.open("b.tex");
    let weak = Rc::downgrade(&hl(&ed));
    let path = ed.document().path().to_path_buf();
    f.state.close_editor(&ed, true);
    common::pump_until(TIMEOUT, || f.state.workspace.get(&path).is_none());
    assert!(highlight::highlighter(&ed).is_none());
    drop(ed);
    common::pump_for(Duration::from_millis(50));
    assert!(
        weak.upgrade().is_none(),
        "nothing keeps the highlighter alive"
    );
    // The other editor is unaffected; reopening highlights again.
    type_at(&other, 0, "$");
    hl(&other).flush();
    window_matches(&other, &hl(&other));
    let ed = f.open("main.tex");
    window_matches(&ed, &hl(&ed));
    // Detaching twice, or an editor never attached, is harmless.
    highlight::detach(&f.state, &ed);
    highlight::detach(&f.state, &ed);
    assert!(highlight::highlighter(&ed).is_none());
    type_at(&ed, 0, "x");
    common::pump_for(Duration::from_millis(20));
}

/// One keystroke as the editor sees it with the default `[typing]` helpers: `$` and `{` insert
/// their pair, typing the closing character over it only moves the cursor (no edit), anything
/// else inserts itself. Returns the edits (offset, text) in order.
fn keystrokes(script: &str, at: usize) -> Vec<(usize, String)> {
    let mut edits = Vec::new();
    let mut at = at;
    // Pairs typed so far whose closing character is still ahead of the cursor.
    let mut pending: Vec<char> = Vec::new();
    for c in script.chars() {
        if pending.last() == Some(&c) {
            pending.pop();
            at += 1;
            continue;
        }
        match c {
            '$' => {
                edits.push((at, "$$".to_owned()));
                pending.push('$');
                at += 1;
            }
            '{' => {
                edits.push((at, "{}".to_owned()));
                pending.push('}');
                at += 1;
            }
            _ => {
                edits.push((at, c.to_string()));
                at += c.len_utf8();
            }
        }
    }
    edits
}

/// The highlighting work per keystroke before the next frame (§7: ≤ 0.5 ms p99 on `large.tex`):
/// the highlighter's change observers plus its tagging of the visible lines. The rest of the
/// window is tagged in the background (reported separately). Typing models the default typing
/// helpers (pairs for `$` and `{`; environments inserted whole, as `\end` auto-close does).
fn perf_keystroke() {
    let f = fixture("perf", &[("large.tex", &fixture_text("large.tex"))]);
    let ed = f.open("large.tex");
    let h = hl(&ed);
    let doc = ed.document().clone();
    // The middle of the document, on screen.
    let line = 30_002;
    let iter = doc.iter_at_byte(doc.line_index().line_start(line)).unwrap();
    let (y, _) = ed.view().line_yrange(&iter);
    let adjustment = ed.view().vadjustment().unwrap();
    adjustment.set_value(f64::from(y) - 100.0);
    h.flush();
    assert!(
        h.window().unwrap().visible.contains(&line),
        "{:?}",
        h.window()
    );
    let script = "We have $x_{n} \\to \\alpha$ by \\cite{key} and \\ref{eq:1}, so \\emph{every} \
                  map $f\\colon X \\to Y$ is flat.\nHence $H^{i}(X, \\mathcal{F}) = 0$ for \
                  $i > 0$, as in \\eqref{eq:2}. % checked\n";
    let mut costs = Vec::new();
    let mut background = Duration::ZERO;
    let mut totals = Vec::new();
    let buffer = ed.buffer().clone();
    let mut keystroke = |edit: &TextEdit, costs: &mut Vec<Duration>| {
        let work = h.work_time();
        let t = Instant::now();
        // As typing does: straight into the buffer.
        let mut start = doc.iter_at_byte(edit.range.start).unwrap();
        if !edit.range.is_empty() {
            let mut end = doc.iter_at_byte(edit.range.end).unwrap();
            buffer.delete(&mut start, &mut end);
        }
        if !edit.text.is_empty() {
            buffer.insert(&mut start, &edit.text);
        }
        h.flush_visible();
        totals.push(t.elapsed());
        costs.push(h.work_time() - work);
        let work = h.work_time();
        h.flush();
        background += h.work_time() - work;
    };
    let mut at = doc.line_index().line_start(line);
    for _round in 0..4 {
        for (offset, text) in keystrokes(script, at) {
            keystroke(&TextEdit::insert(offset, text.clone()), &mut costs);
            at = offset + text.len();
        }
        // Backspace over the last line typed (ASCII).
        for _ in 0..30 {
            keystroke(&TextEdit::delete(at - 1..at), &mut costs);
            at -= 1;
        }
        // An environment, inserted whole (snippet or `\end` auto-close).
        keystroke(
            &TextEdit::insert(at, "\\begin{align}\n  a &= b\n\\end{align}\n"),
            &mut costs,
        );
        at += "\\begin{align}\n  a &= b\n\\end{align}\n".len();
    }
    let p = |v: &mut Vec<Duration>, q: f64| {
        v.sort();
        v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
    };
    let (p50, p99, max) = (
        p(&mut costs, 0.5),
        p(&mut costs, 0.99),
        *costs.last().unwrap(),
    );
    let (t50, t99) = (p(&mut totals, 0.5), p(&mut totals, 0.99));
    println!(
        "    highlighting per keystroke on large.tex ({} keys): p50 {p50:?}, p99 {p99:?}, max \
         {max:?}; background tagging {background:?} in all; whole keystroke (buffer, document, \
         scanner, highlighting) p50 {t50:?}, p99 {t99:?}",
        costs.len()
    );
    // For the record: a change that recolours the whole screen (an unclosed environment, which
    // the typing helpers' `\end` auto-close avoids) and a scroll by one screen.
    let work = h.work_time();
    let at = doc.line_index().line_start(line);
    doc.apply_edits(&[TextEdit::insert(at, "\\begin{equation}\n")], Origin::User)
        .unwrap();
    h.flush_visible();
    let recolour = h.work_time() - work;
    let work = h.work_time();
    h.flush();
    let recolour_rest = h.work_time() - work;
    let work = h.work_time();
    adjustment.set_value(adjustment.value() + 3.0 * adjustment.page_size());
    h.flush();
    println!(
        "    whole-screen recolour: {recolour:?} before the frame + {recolour_rest:?} in the \
         background; scrolling to an untagged screen: {:?}",
        h.work_time() - work
    );
    // Memory of the worst case: every line of large.tex visited, so tagged (printed only).
    let rss = || {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            })
            .unwrap_or(0)
    };
    let before = rss();
    let t = Instant::now();
    h.tag_all();
    println!(
        "    tagging all {} lines: {:?}, RSS +{} MB",
        doc.line_index().line_count(),
        t.elapsed(),
        rss().saturating_sub(before) / 1024
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(p99 <= Duration::from_micros(500), "p99 {p99:?} over 0.5 ms");
    }
}
