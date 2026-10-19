//! WP3.9: delimiter pairs (`editor/pairs.rs`): the pair table on a fixture, the environments
//! against the structure index, the highlight (tags, following the cursor, the text, the Vim
//! mode and the style scheme), unmatched delimiters, `%` in Vim normal mode and Ctrl+%
//! (in-process and on real key events), detaching, and the `--demo pairs` function.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_pairs`.
//! The cost per cursor move (≤ 0.5 ms on `large.tex`) runs in release:
//! `scripts/broadway.sh cargo test --release -p lemma-app --test gtk_pairs -- --ignored perf`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `pairs-light.png` and `pairs-dark.png` there.

mod broadway_input;
mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::pairs::{self, Jump, MATCH_TAG, PairKind, Partner, Side, UNMATCHED_TAG};
use lemma_app::editor::{Editor, VimMode, vim};
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::{Origin, ProjectRoot, TextEdit};
use sourceview5::prelude::*;
use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    isolate_environment();
    if broadway_input::is_child() {
        return common::run(&[("pairs_real_keys_child", real_keys::child)]);
    }
    let mut tests: Vec<common::Test> = vec![
        ("pair_table_on_a_fixture", pair_table),
        (
            "environments_agree_with_the_structure_index",
            structure_index,
        ),
        ("highlight_follows_the_cursor", highlight_follows_cursor),
        ("highlight_follows_edits_and_undo", highlight_follows_edits),
        ("unmatched_delimiters_are_tinted", unmatched),
        ("highlight_side_follows_the_vim_mode", vim_mode_side),
        ("tags_follow_the_style_scheme", style_scheme),
        ("bracket_matching_is_ours_in_latex_only", bracket_matching),
        ("vim_percent_jumps_and_falls_back", vim_percent),
        ("ctrl_percent_jumps_outside_vim", ctrl_percent),
        ("jumps_scroll_to_far_partners", far_jump),
        ("lone_carriage_returns_and_crlf", carriage_returns),
        ("detach_removes_everything", detach),
        ("demo_highlights_a_pair", demo),
        ("pairs_on_real_key_events", real_keys::parent),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_pairs_cursor_moves_large_tex", perf_cursor_moves));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(30);

/// Points the XDG directories and the config file into a temporary directory, so nothing reads
/// or writes the user's. Runs before GTK starts any thread.
fn isolate_environment() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-pairs-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // SAFETY: single-threaded: GTK is not initialised yet and no thread was spawned.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_STATE_HOME", root.join("state"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        std::env::set_var("LEMMA_CONFIG", root.join("config.toml"));
    }
}

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
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
    config.spell.enabled = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        let doc = editor.document().clone();
        common::pump_until(TIMEOUT, || doc.has_line_states());
        editor
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// The byte offset of the first `needle` in `editor`'s text, plus `delta`.
fn offset_of(editor: &Editor, needle: &str, delta: usize) -> usize {
    editor
        .document()
        .text()
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is in the text"))
        + delta
}

/// Puts the cursor at `offset` and brings the highlight up to date.
fn cursor_to(editor: &Editor, offset: usize) {
    assert!(editor.place_cursor_at(offset));
    pairs::refresh_now(editor);
}

/// The ranges that carry `tag` in `editor`'s buffer, read from the buffer (document bytes).
fn tagged(editor: &Editor, tag: &str) -> Vec<Range<usize>> {
    let buffer = editor.buffer();
    let Some(tag) = buffer.tag_table().lookup(tag) else {
        return Vec::new();
    };
    let doc = editor.document();
    let mut out = Vec::new();
    let mut iter = buffer.start_iter();
    loop {
        if iter.starts_tag(Some(&tag)) {
            let start = doc.byte_offset(&iter);
            let mut end = iter;
            end.forward_to_tag_toggle(Some(&tag));
            out.push(start..doc.byte_offset(&end));
            iter = end;
            continue;
        }
        if !iter.forward_to_tag_toggle(Some(&tag)) {
            break;
        }
    }
    out
}

/// The texts of the ranges carrying `tag`.
fn tagged_texts(editor: &Editor, tag: &str) -> Vec<String> {
    let text = editor.document().text();
    tagged(editor, tag)
        .into_iter()
        .map(|r| text[r].to_owned())
        .collect()
}

// ----- the pair table -----

/// A document with every kind of pair, nested, plus comments and verbatim text.
const TABLE_DOC: &str = r"\documentclass{article}
\usepackage{amsmath}
\begin{document}
\section{Pairs}\label{sec:pairs}
\begin{theorem}[Nested]
  \begin{itemize}
    \item $\left. f \right|_{U}$ and \(\bigl( x \bigr)\)
    \item \verb|\end{itemize}| % \end{theorem} {
  \end{itemize}
  \begin{align*}
    \left[ \sum_{i} a_i \right] &= \Bigl\{ x \Bigr\} \\
    y &= \langle u, v \rangle + \{ w \}
  \end{align*}
\end{theorem}
\begin{verbatim}
\begin{theorem} { $
\end{verbatim}
\[
  E = mc^2 \quad \text{if $m > 0$}
\]
$$ a $$ and $x$$y$ (see [1])
\end{document}
";

/// One row: the cursor at `at` (+ `delta`), the partner at `partner` (+ `pdelta`; `None`: the
/// delimiter is unmatched), the kind.
struct Row {
    at: &'static str,
    delta: usize,
    kind: PairKind,
    partner: Option<(&'static str, usize)>,
}

const fn row(
    at: &'static str,
    delta: usize,
    kind: PairKind,
    partner: &'static str,
    pdelta: usize,
) -> Row {
    Row {
        at,
        delta,
        kind,
        partner: Some((partner, pdelta)),
    }
}

const TABLE: &[Row] = &[
    row("\\begin{document}", 0, PairKind::Env, "\\end{document}", 0),
    row("\\begin{document}", 9, PairKind::Env, "\\end{document}", 0),
    row("\\end{document}", 0, PairKind::Env, "\\begin{document}", 0),
    row("{Pairs}", 0, PairKind::Brace, "}\\label{sec", 0),
    row("[Nested]", 0, PairKind::Bracket, "]\n  \\begin{itemize}", 0),
    row(
        "\\begin{theorem}[Nested]",
        0,
        PairKind::Env,
        "\\end{theorem}\n\\begin{verbatim}",
        0,
    ),
    row(
        "\\begin{itemize}",
        0,
        PairKind::Env,
        "\\end{itemize}\n  \\begin{align*}",
        0,
    ),
    row(
        "\\end{itemize}\n  \\begin{align*}",
        3,
        PairKind::Env,
        "\\begin{itemize}",
        0,
    ),
    row("\\left. f", 0, PairKind::LeftRight, "\\right|_{U}", 0),
    row("\\right|_{U}", 6, PairKind::LeftRight, "\\left. f", 0),
    row("{U}", 0, PairKind::Brace, "}$ and", 0),
    row("$\\left. f", 0, PairKind::Dollar, "$ and \\(", 0),
    row("\\(\\bigl", 0, PairKind::MathParen, "\\)\n    \\item", 0),
    row("\\bigl( x", 0, PairKind::Sized, "\\bigr)", 0),
    row("\\bigl( x", 5, PairKind::Sized, "\\bigr)", 0),
    row("\\begin{align*}", 0, PairKind::Env, "\\end{align*}", 0),
    row("\\left[ \\sum", 0, PairKind::LeftRight, "\\right] &=", 0),
    row("\\right] &=", 0, PairKind::LeftRight, "\\left[ \\sum", 0),
    row("\\Bigl\\{", 0, PairKind::Sized, "\\Bigr\\}", 0),
    row("\\langle u", 0, PairKind::Named(0), "\\rangle +", 0),
    row(
        "\\{ w",
        0,
        PairKind::EscapedBrace,
        "\\}\n  \\end{align*}",
        0,
    ),
    row("{i}", 0, PairKind::Brace, "} a_i", 0),
    row("\\begin{verbatim}", 0, PairKind::Env, "\\end{verbatim}", 0),
    row("\\[\n  E", 0, PairKind::MathBracket, "\\]\n$$", 0),
    row("$m > 0$", 0, PairKind::Dollar, "$}\n\\]", 0),
    row("{if $m", 0, PairKind::Brace, "}\n\\]", 0),
    row("$$ a $$", 0, PairKind::DoubleDollar, "$$ and", 0),
    row("$x$$y$", 0, PairKind::Dollar, "$$y$", 0),
    row("$x$$y$", 2, PairKind::Dollar, "$x$$y$", 0),
    row("$x$$y$", 3, PairKind::Dollar, "$ (see", 0),
    row("(see [1])", 0, PairKind::Paren, ")\n\\end{document}", 0),
    row("[1])", 0, PairKind::Bracket, "])", 0),
];

/// Delimiters that do not count: in verbatim text and comments, or escaped.
const NOT_DELIMITERS: &[(&str, usize)] = &[
    // `\verb|\end{itemize}|` and `% \end{theorem} {`.
    ("\\end{itemize}| %", 0),
    ("\\end{theorem} {", 0),
    ("\\end{theorem} {", 14),
    // The verbatim body.
    ("\\begin{theorem} { $", 0),
    ("{ $\n\\end{verbatim}", 0),
    ("$\n\\end{verbatim}", 0),
    // Plain text.
    ("Nested]", 0),
];

fn pair_table() {
    // The row texts are unique in the document (or their first occurrence is meant).
    let f = fixture("pair-table", &[("table.tex", TABLE_DOC)]);
    let editor = f.open("table.tex");
    let text = editor.document().text();
    for row in TABLE {
        let at = offset_of(&editor, row.at, row.delta);
        let pair = pairs::pair_at(&editor, at, Side::On)
            .unwrap_or_else(|| panic!("a delimiter at {:?}+{}", row.at, row.delta));
        assert_eq!(pair.kind, row.kind, "{:?}+{}", row.at, row.delta);
        let want = match row.partner {
            Some((needle, pdelta)) => {
                let start = text.find(needle).unwrap_or_else(|| panic!("{needle:?}")) + pdelta;
                let Partner::Found(found) = &pair.partner else {
                    panic!("{:?}+{}: {:?}", row.at, row.delta, pair.partner);
                };
                assert_eq!(
                    found.start,
                    start,
                    "{:?}+{}: partner {:?}",
                    row.at,
                    row.delta,
                    &text[found.clone()]
                );
                Partner::Found(found.clone())
            }
            None => Partner::Unmatched,
        };
        assert_eq!(pair.partner, want, "{:?}+{}", row.at, row.delta);
        // And back from the partner.
        if let Partner::Found(found) = &pair.partner {
            let back = pairs::pair_at(&editor, found.start, Side::On).unwrap();
            assert_eq!(
                back.partner,
                Partner::Found(pair.token.clone()),
                "back from {:?}",
                &text[found.clone()]
            );
            assert_eq!(back.open, !pair.open);
        }
    }
    for (needle, delta) in NOT_DELIMITERS {
        let at = offset_of(&editor, needle, *delta);
        assert_eq!(
            pairs::pair_at(&editor, at, Side::On),
            None,
            "{needle:?}+{delta}"
        );
    }
}

/// Every environment the structure index (`lemma_syntax::index_file`, `EnvPair`) pairs on the
/// fixtures pairs the same way in the editor.
fn structure_index() {
    let proj = common::fixtures().join("proj");
    let main = std::fs::read_to_string(proj.join("main.tex")).unwrap();
    let chapter = std::fs::read_to_string(proj.join("chapters/yoneda.tex")).unwrap();
    let f = fixture(
        "structure-index",
        &[
            ("main.tex", main.as_str()),
            ("chapters/yoneda.tex", chapter.as_str()),
            ("table.tex", TABLE_DOC),
        ],
    );
    let mut checked = 0;
    for file in ["main.tex", "chapters/yoneda.tex", "table.tex"] {
        let editor = f.open(file);
        let text = editor.document().text();
        let index = lemma_syntax::index_file(&text);
        let lines: Vec<&str> = text.split('\n').collect();
        let line_start =
            |l: u32| -> usize { lines[..l as usize].iter().map(|s| s.len() + 1).sum() };
        for env in &index.envs {
            let begin = format!("\\begin{{{}}}", env.name);
            let end = format!("\\end{{{}}}", env.name);
            let begin_line = lines[env.begin_line as usize];
            let at = line_start(env.begin_line) + begin_line.find(&begin).unwrap();
            let pair = pairs::pair_at(&editor, at, Side::On).expect("a delimiter");
            match env.end_line {
                Some(end_line) => {
                    let want = line_start(end_line) + lines[end_line as usize].find(&end).unwrap();
                    assert_eq!(
                        pair.partner,
                        Partner::Found(want..want + end.len()),
                        "{file}: {} at line {}",
                        env.name,
                        env.begin_line + 1
                    );
                }
                None => assert_eq!(pair.partner, Partner::Unmatched, "{file}: {}", env.name),
            }
            checked += 1;
        }
    }
    assert!(checked >= 20, "{checked} environments checked");
}

// ----- the highlight -----

const DOC: &str = "\\documentclass{article}\n\
\\begin{document}\n\
\\begin{theorem}\n\
Let $f(x) = \\left( \\frac{a}{b} \\right)$.\n\
\\end{theorem}\n\
Plain text here.\n\
\\end{document}\n";

fn highlight_follows_cursor() {
    let f = fixture("highlight-cursor", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    cursor_to(&editor, offset_of(&editor, "\\begin{theorem}", 3));
    assert_eq!(
        tagged_texts(&editor, MATCH_TAG),
        ["\\begin{theorem}", "\\end{theorem}"]
    );
    assert!(tagged(&editor, UNMATCHED_TAG).is_empty());
    let (ranges, matched) = pairs::highlighted(&editor).unwrap();
    assert!(matched);
    assert_eq!(ranges, tagged(&editor, MATCH_TAG));
    // Through the idle a cursor move queues, as when the user moves it.
    editor.place_cursor_at(offset_of(&editor, "\\frac{a}", 5));
    common::pump_until(TIMEOUT, || tagged_texts(&editor, MATCH_TAG) == ["{", "}"]);
    assert_eq!(
        tagged(&editor, MATCH_TAG)[0].start,
        offset_of(&editor, "{a}", 0)
    );
    // `\left(` … `\right)`.
    cursor_to(&editor, offset_of(&editor, "\\right)", 0));
    assert_eq!(tagged_texts(&editor, MATCH_TAG), ["\\left(", "\\right)"]);
    // The `(` of `f(x)`.
    cursor_to(&editor, offset_of(&editor, "(x)", 0));
    assert_eq!(tagged_texts(&editor, MATCH_TAG), ["(", ")"]);
    // `$` … `$`.
    cursor_to(&editor, offset_of(&editor, "$f(x)", 0));
    assert_eq!(tagged_texts(&editor, MATCH_TAG), ["$", "$"]);
    // Plain text: nothing.
    cursor_to(&editor, offset_of(&editor, "Plain", 2));
    assert!(tagged(&editor, MATCH_TAG).is_empty());
    assert!(tagged(&editor, UNMATCHED_TAG).is_empty());
    assert_eq!(pairs::highlighted(&editor), None);
    // No highlight is re-applied when nothing changed: the marks stay the same.
    cursor_to(&editor, offset_of(&editor, "\\end{document}", 0));
    let first = pairs::highlighted(&editor);
    cursor_to(&editor, offset_of(&editor, "\\end{document}", 4));
    assert_eq!(pairs::highlighted(&editor), first);
    assert_eq!(
        tagged_texts(&editor, MATCH_TAG),
        ["\\begin{document}", "\\end{document}"]
    );
}

fn highlight_follows_edits() {
    let f = fixture("highlight-edits", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    cursor_to(&editor, offset_of(&editor, "\\begin{theorem}", 0));
    assert_eq!(tagged_texts(&editor, MATCH_TAG).len(), 2);
    // An edit (not at the cursor) renames the end: the begin is unmatched now.
    let end = offset_of(&editor, "\\end{theorem}", 5);
    doc.apply_edits(&[TextEdit::insert(end, "x")], Origin::User)
        .unwrap();
    common::pump_until(TIMEOUT, || {
        tagged_texts(&editor, UNMATCHED_TAG) == ["\\begin{theorem}"]
    });
    assert!(tagged(&editor, MATCH_TAG).is_empty(), "old tags are gone");
    // Undo brings the pair back.
    editor.buffer().undo();
    common::pump_until(TIMEOUT, || tagged_texts(&editor, MATCH_TAG).len() == 2);
    assert_eq!(
        tagged_texts(&editor, MATCH_TAG),
        ["\\begin{theorem}", "\\end{theorem}"]
    );
    assert!(tagged(&editor, UNMATCHED_TAG).is_empty());
    // Typing in front of the pair moves both highlights with the text.
    let buffer = editor.buffer();
    let mut start = buffer.start_iter();
    buffer.insert(&mut start, "% new first line\n");
    cursor_to(&editor, offset_of(&editor, "\\begin{theorem}", 0));
    assert_eq!(
        tagged(&editor, MATCH_TAG),
        [
            offset_of(&editor, "\\begin{theorem}", 0)..offset_of(&editor, "\\begin{theorem}", 15),
            offset_of(&editor, "\\end{theorem}", 0)..offset_of(&editor, "\\end{theorem}", 13),
        ]
    );
    // A verbatim environment opened above hides both: no highlight.
    let at = offset_of(&editor, "\\begin{theorem}", 0);
    doc.apply_edits(&[TextEdit::insert(at, "\\begin{verbatim}\n")], Origin::User)
        .unwrap();
    cursor_to(&editor, offset_of(&editor, "\\begin{theorem}", 0));
    assert!(tagged(&editor, MATCH_TAG).is_empty());
    assert!(tagged(&editor, UNMATCHED_TAG).is_empty());
    // The tags never enter the undo history or mark the buffer modified.
    buffer.set_modified(false);
    cursor_to(&editor, offset_of(&editor, "\\begin{verbatim}", 0));
    assert!(!buffer.is_modified());
}

fn unmatched() {
    let doc = "\\begin{document}\n\
               A {group and $x$ (see\n\
               \n\
               b) here.\n\
               \\begin{proof} no end.\n\
               \\end{document}\n";
    let f = fixture("unmatched", &[("main.tex", doc)]);
    let editor = f.open("main.tex");
    cursor_to(&editor, offset_of(&editor, "{group", 0));
    // A brace goes on past the blank line, to the end of the document.
    assert_eq!(tagged_texts(&editor, UNMATCHED_TAG), ["{"]);
    assert!(tagged(&editor, MATCH_TAG).is_empty());
    let (_, matched) = pairs::highlighted(&editor).unwrap();
    assert!(!matched);
    // A parenthesis stops at the blank line.
    cursor_to(&editor, offset_of(&editor, "(see", 0));
    assert_eq!(tagged_texts(&editor, UNMATCHED_TAG), ["("]);
    cursor_to(&editor, offset_of(&editor, "b) here", 1));
    assert_eq!(tagged_texts(&editor, UNMATCHED_TAG), [")"]);
    cursor_to(&editor, offset_of(&editor, "\\begin{proof}", 2));
    assert_eq!(tagged_texts(&editor, UNMATCHED_TAG), ["\\begin{proof}"]);
    // A matched pair on the same line is still a match.
    cursor_to(&editor, offset_of(&editor, "$x$", 0));
    assert_eq!(tagged_texts(&editor, MATCH_TAG), ["$", "$"]);
    assert!(tagged(&editor, UNMATCHED_TAG).is_empty());
}

fn vim_mode_side() {
    let f = fixture_with("vim-side", &[("main.tex", DOC)], |c| c.editor.vim = true);
    let editor = f.open("main.tex");
    editor.view().grab_focus();
    common::pump_until(TIMEOUT, || editor.view().has_focus());
    assert_eq!(editor.vim_status().unwrap().mode, VimMode::Normal);
    // Normal mode: the block cursor's character only; after `}` there is nothing.
    let after = offset_of(&editor, "{b}", 3);
    cursor_to(&editor, after);
    assert!(tagged(&editor, MATCH_TAG).is_empty());
    // Insert mode: the character before the cursor counts too.
    let im = editor.vim().unwrap();
    vim::feed_key(&im, editor.view(), gdk::Key::i, gdk::ModifierType::empty());
    assert_eq!(editor.vim_status().unwrap().mode, VimMode::Insert);
    cursor_to(&editor, after);
    assert_eq!(tagged_texts(&editor, MATCH_TAG), ["{", "}"]);
    // Plain editing (Vim off) too.
    f.state.set_vim_enabled(false);
    cursor_to(&editor, after);
    assert_eq!(tagged_texts(&editor, MATCH_TAG), ["{", "}"]);
}

fn style_scheme() {
    let f = fixture("style-scheme", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let buffer = editor.buffer();
    cursor_to(&editor, offset_of(&editor, "{a}", 0));
    let table = buffer.tag_table();
    let tag = table.lookup(MATCH_TAG).unwrap();
    let manager = sourceview5::StyleSchemeManager::default();
    let rgba = |tag: &gtk::TextTag| tag.background_rgba().unwrap();
    // Adwaita: the scheme's match style is bold only: an accent tint, bold.
    buffer.set_style_scheme(manager.scheme("Adwaita").as_ref());
    let light = rgba(&tag);
    assert!(tag.is_background_set());
    assert!(light.alpha() > 0.1 && light.alpha() < 0.5, "{light:?}");
    assert_eq!(tag.weight(), 700);
    buffer.set_style_scheme(manager.scheme("Adwaita-dark").as_ref());
    let dark = rgba(&tag);
    assert!(dark.alpha() > light.alpha(), "{dark:?} {light:?}");
    // A scheme with a background for matching brackets: its colours.
    if let Some(classic) = manager.scheme("classic") {
        let style = classic.style("bracket-match").unwrap();
        buffer.set_style_scheme(Some(&classic));
        let want = gdk::RGBA::parse(style.background().unwrap().as_str()).unwrap();
        assert_eq!(rgba(&tag), want);
    }
    // The unmatched tint is red, light and subtle.
    buffer.set_style_scheme(manager.scheme("Adwaita").as_ref());
    let unmatched = table.lookup(UNMATCHED_TAG).unwrap();
    let tint = rgba(&unmatched);
    assert!(
        tint.red() > 0.8 && tint.green() < 0.3 && tint.alpha() < 0.3,
        "{tint:?}"
    );
    snapshots(&f, &editor);
}

/// `LEMMA_TEST_SNAPSHOTS=dir`: a matched and an unmatched pair in light and dark.
fn snapshots(f: &Fixture, editor: &Editor) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let manager = sourceview5::StyleSchemeManager::default();
    for (scheme, name) in [("Adwaita", "light"), ("Adwaita-dark", "dark")] {
        let style = adw::StyleManager::default();
        style.set_color_scheme(if name == "dark" {
            adw::ColorScheme::ForceDark
        } else {
            adw::ColorScheme::ForceLight
        });
        editor
            .buffer()
            .set_style_scheme(manager.scheme(scheme).as_ref());
        cursor_to(editor, offset_of(editor, "\\left(", 0));
        let png = dir.join(format!("pairs-{name}.png"));
        block_on(testmode::snapshot_next_frame(&f.state.window, &png)).unwrap();
        // An environment whose end is misspelt.
        let doc = editor.document();
        let end = offset_of(editor, "\\end{theorem}", 5);
        doc.apply_edits(&[TextEdit::insert(end, "x")], Origin::User)
            .unwrap();
        cursor_to(editor, offset_of(editor, "\\begin{theorem}", 0));
        let png = dir.join(format!("pairs-unmatched-{name}.png"));
        block_on(testmode::snapshot_next_frame(&f.state.window, &png)).unwrap();
        editor.buffer().undo();
    }
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
}

fn bracket_matching() {
    let f = fixture(
        "bracket-matching",
        &[("main.tex", DOC), ("refs.bib", "@book{k, title={A {B}}}\n")],
    );
    let editor = f.open("main.tex");
    assert!(!editor.buffer().is_highlight_matching_brackets());
    let bib = f.open("refs.bib");
    assert!(bib.buffer().is_highlight_matching_brackets());
    cursor_to(&bib, 0);
    bib.place_cursor_at(offset_of(&bib, "{k", 0));
    common::pump_for(Duration::from_millis(30));
    assert!(tagged(&bib, MATCH_TAG).is_empty());
    assert_eq!(pairs::highlighted(&bib), None);
    // Detaching gives it back.
    pairs::detach(&f.state, &editor);
    assert!(editor.buffer().is_highlight_matching_brackets());
}

// ----- jumping -----

fn key(editor: &Editor, keyval: gdk::Key) -> glib::Propagation {
    let event: KeyEvent = keys::event_for(editor, keyval, gdk::ModifierType::empty());
    keys::dispatch(editor.view(), &event)
}

fn vim_percent() {
    let f = fixture_with("vim-percent", &[("main.tex", DOC)], |c| c.editor.vim = true);
    let editor = f.open("main.tex");
    editor.view().grab_focus();
    common::pump_until(TIMEOUT, || editor.view().has_focus());
    let im = editor.vim().unwrap();
    let begin = offset_of(&editor, "\\begin{theorem}", 4);
    let end = offset_of(&editor, "\\end{theorem}", 0);
    editor.place_cursor_at(begin);
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Stop);
    assert_eq!(editor.cursor_offset(), end);
    // Back to the start of the begin.
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Stop);
    assert_eq!(editor.cursor_offset(), begin - 4);
    // Not on a delimiter: the next one on the line (Vim's `%` looks forward).
    editor.place_cursor_at(offset_of(&editor, "Let", 0));
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Stop);
    assert_eq!(editor.cursor_offset(), offset_of(&editor, "$.", 0));
    // Nothing on the rest of the line: Vim's own `%`.
    editor.place_cursor_at(offset_of(&editor, "Plain", 0));
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Proceed);
    // A count (`50%`), an operator (`d%`), an invisible operator (`y%`), a replace (`r%`): Vim's.
    let none = gdk::ModifierType::empty();
    let on_brace = offset_of(&editor, "{a}", 0);
    for (keys_before, what) in [
        (&[gdk::Key::_5, gdk::Key::_0][..], "a count"),
        (&[gdk::Key::d][..], "an operator"),
    ] {
        editor.place_cursor_at(on_brace);
        for k in keys_before {
            assert_eq!(key(&editor, *k), glib::Propagation::Proceed);
            vim::feed_key(&im, editor.view(), *k, none);
        }
        assert!(!editor.vim_status().unwrap().command.is_empty(), "{what}");
        assert_eq!(
            key(&editor, gdk::Key::percent),
            glib::Propagation::Proceed,
            "{what}"
        );
        vim::cancel_pending(&im, editor.view());
    }
    for invisible in [gdk::Key::y, gdk::Key::r] {
        editor.place_cursor_at(on_brace);
        assert_eq!(key(&editor, invisible), glib::Propagation::Proceed);
        vim::feed_key(&im, editor.view(), invisible, none);
        assert_eq!(
            key(&editor, gdk::Key::percent),
            glib::Propagation::Proceed,
            "{invisible:?}"
        );
        assert_eq!(editor.cursor_offset(), on_brace);
        vim::feed_key(&im, editor.view(), gdk::Key::Escape, none);
    }
    // After those, a plain `%` jumps again.
    editor.place_cursor_at(on_brace);
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Stop);
    assert_eq!(editor.cursor_offset(), offset_of(&editor, "}{b}", 0));
    // An unmatched delimiter: the key is used (the bell rings), the cursor stays.
    let doc = editor.document().clone();
    let at = offset_of(&editor, "Plain", 0);
    doc.apply_edits(&[TextEdit::insert(at, "[")], Origin::User)
        .unwrap();
    editor.place_cursor_at(at);
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Stop);
    assert_eq!(editor.cursor_offset(), at);
    // Insert mode: `%` is a character.
    vim::feed_key(&im, editor.view(), gdk::Key::i, none);
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Proceed);
    // Visual mode: Vim's own `%`.
    vim::feed_key(&im, editor.view(), gdk::Key::Escape, none);
    vim::feed_key(&im, editor.view(), gdk::Key::v, none);
    assert_eq!(editor.vim_status().unwrap().mode, VimMode::Visual);
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Proceed);
}

fn ctrl_percent() {
    let f = fixture("ctrl-percent", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    assert!(editor.vim().is_none());
    // Before `{`: to before `}`, and back.
    let open = offset_of(&editor, "{a}", 0);
    editor.place_cursor_at(open);
    WidgetExt::activate_action(&f.state.window, "win.jump-pair", None).unwrap();
    assert_eq!(editor.cursor_offset(), open + 2);
    assert_eq!(pairs::jump_in(&editor), Jump::Moved);
    assert_eq!(editor.cursor_offset(), open);
    // After `)` (the character before the cursor).
    let close = offset_of(&editor, "(x)", 3);
    editor.place_cursor_at(close);
    assert_eq!(pairs::jump_in(&editor), Jump::Moved);
    assert_eq!(editor.cursor_offset(), offset_of(&editor, "(x)", 0));
    // The character after the cursor wins: after `\right)` comes the closing `$`.
    editor.place_cursor_at(offset_of(&editor, "\\right)", 7));
    assert_eq!(pairs::jump_in(&editor), Jump::Moved);
    assert_eq!(editor.cursor_offset(), offset_of(&editor, "$f(x)", 0));
    // On plain text: the next delimiter on the line.
    editor.place_cursor_at(offset_of(&editor, "Let", 1));
    assert_eq!(pairs::jump_in(&editor), Jump::Moved);
    assert_eq!(editor.cursor_offset(), offset_of(&editor, "$.", 0));
    // Nothing: the bell, the cursor stays.
    let plain = offset_of(&editor, "Plain", 1);
    editor.place_cursor_at(plain);
    assert_eq!(pairs::jump_in(&editor), Jump::Nothing);
    WidgetExt::activate_action(&f.state.window, "win.jump-pair", None).unwrap();
    assert_eq!(editor.cursor_offset(), plain);
    // In Vim visual mode the selection grows to the partner.
    f.state.set_vim_enabled(true);
    editor.view().grab_focus();
    common::pump_until(TIMEOUT, || editor.view().has_focus());
    let im = editor.vim().unwrap();
    let begin = offset_of(&editor, "\\begin{theorem}", 0);
    editor.place_cursor_at(begin);
    vim::feed_key(&im, editor.view(), gdk::Key::v, gdk::ModifierType::empty());
    assert_eq!(editor.vim_status().unwrap().mode, VimMode::Visual);
    assert_eq!(pairs::jump_in(&editor), Jump::Moved);
    let (a, b) = editor.buffer().selection_bounds().expect("a selection");
    let doc = editor.document();
    assert_eq!(doc.byte_offset(&a), begin);
    assert!(doc.byte_offset(&b) >= offset_of(&editor, "\\end{theorem}", 0));
}

fn far_jump() {
    let body = "A line of text.\n".repeat(400);
    let text =
        format!("\\begin{{document}}\n\\begin{{proof}}\n{body}\\end{{proof}}\n\\end{{document}}\n");
    let f = fixture("far-jump", &[("main.tex", &text)]);
    let editor = f.open("main.tex");
    let view = editor.view().clone();
    editor.place_cursor_at(offset_of(&editor, "\\begin{proof}", 0));
    view.scroll_to_mark(&editor.buffer().get_insert(), 0.0, true, 0.0, 0.0);
    common::pump_for(Duration::from_millis(50));
    assert_eq!(pairs::jump_in(&editor), Jump::Moved);
    let end = offset_of(&editor, "\\end{proof}", 0);
    assert_eq!(editor.cursor_offset(), end);
    // The partner is on screen once the view settled.
    common::pump_until(TIMEOUT, || {
        let rect = view.iter_location(&editor.iter_near(end).unwrap());
        let visible = view.visible_rect();
        rect.y() >= visible.y() && rect.y() + rect.height() <= visible.y() + visible.height()
    });
    pairs::refresh_now(&editor);
    assert_eq!(
        tagged_texts(&editor, MATCH_TAG),
        ["\\begin{proof}", "\\end{proof}"]
    );
}

/// GTK breaks lines at a lone `\r` (the document's line index does not), and CRLF files keep
/// their `\r`s: the pairs are the same.
fn carriage_returns() {
    let lone = "\\begin{document}\nx\ry {a}\r(b)\n\n\\end{document}\n";
    let crlf = "\\begin{document}\r\n$x$ {a}\r\n\r\n(b\r\n\\end{document}\r\n";
    let f = fixture(
        "carriage-returns",
        &[("lone.tex", lone), ("crlf.tex", crlf)],
    );
    for file in ["lone.tex", "crlf.tex"] {
        let editor = f.open(file);
        cursor_to(&editor, offset_of(&editor, "{a}", 0));
        assert_eq!(tagged_texts(&editor, MATCH_TAG), ["{", "}"], "{file}");
        cursor_to(&editor, offset_of(&editor, "\\begin{document}", 0));
        assert_eq!(
            tagged_texts(&editor, MATCH_TAG),
            ["\\begin{document}", "\\end{document}"],
            "{file}"
        );
        cursor_to(&editor, offset_of(&editor, "\\end{document}", 1));
        assert_eq!(
            tagged(&editor, MATCH_TAG)[0].start,
            offset_of(&editor, "\\begin{document}", 0),
            "{file}"
        );
    }
    // A blank line of `\r\n` ends the parenthesis' paragraph.
    let editor = f.open("crlf.tex");
    cursor_to(&editor, offset_of(&editor, "(b", 0));
    assert_eq!(tagged_texts(&editor, UNMATCHED_TAG), ["("]);
    let editor = f.open("lone.tex");
    cursor_to(&editor, offset_of(&editor, "(b)", 0));
    assert_eq!(tagged_texts(&editor, MATCH_TAG), ["(", ")"]);
}

fn detach() {
    let f = fixture_with(
        "detach",
        &[("main.tex", DOC), ("b.tex", "\\textbf{x}\n")],
        |c| c.editor.vim = true,
    );
    let editor = f.open("main.tex");
    cursor_to(&editor, offset_of(&editor, "\\begin{theorem}", 0));
    assert_eq!(tagged_texts(&editor, MATCH_TAG).len(), 2);
    pairs::detach(&f.state, &editor);
    assert!(tagged(&editor, MATCH_TAG).is_empty());
    assert_eq!(pairs::highlighted(&editor), None);
    // `%` is Vim's again, and moving the cursor highlights nothing.
    let at = offset_of(&editor, "{a}", 0);
    editor.place_cursor_at(at);
    assert_eq!(key(&editor, gdk::Key::percent), glib::Propagation::Proceed);
    common::pump_for(Duration::from_millis(30));
    assert!(tagged(&editor, MATCH_TAG).is_empty());
    // Closing a tab detaches it (the window's hook): the editor is freed.
    let other = f.open("b.tex");
    cursor_to(&other, offset_of(&other, "{x}", 0));
    assert_eq!(tagged_texts(&other, MATCH_TAG), ["{", "}"]);
    let weak = Rc::downgrade(&other);
    f.state.close_editor(&other, true);
    drop(other);
    common::pump_until(TIMEOUT, || weak.upgrade().is_none());
}

fn demo() {
    let f = fixture("demo", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    block_on(pairs::run_demo(f.state.clone())).unwrap();
    // The first environment after `\begin{document}` whose end is near.
    assert_eq!(
        tagged_texts(&editor, MATCH_TAG),
        ["\\begin{theorem}", "\\end{theorem}"]
    );
    assert_eq!(
        editor.cursor_offset(),
        offset_of(&editor, "\\begin{theorem}", 0)
    );
}

// ----- real key events -----

mod real_keys {
    //! `%` and Ctrl+% on real key events (a private Broadway daemon, see `tests/broadway_input`).

    use super::*;
    use broadway_input::{Browser, CONTROL};

    pub fn parent() {
        broadway_input::run_child("pairs_real_keys_child", 39);
    }

    pub fn child() {
        let text = "\\begin{document}\n\
                    \\begin{proof}\n\
                    Take $\\frac{a}{b}$ and (x) here.\n\
                    \\end{proof}\n\
                    \\end{document}\n";
        let f = fixture_with("real-keys", &[("main.tex", text)], |c| c.editor.vim = true);
        let editor = f.open("main.tex");
        lemma_app::app::set_accels(&f.state.app, &f.state.config.keys);
        let view = editor.view().clone();
        f.state.sidebar.split.set_show_sidebar(false);
        common::pump_until(TIMEOUT, || f.state.window.is_active());
        view.grab_focus();
        common::pump_until(TIMEOUT, || view.has_focus());
        let mut browser = Browser::connect();
        browser.settle = Duration::from_millis(20);
        let wait_cursor = |offset: usize| {
            common::pump_until(TIMEOUT, || editor.cursor_offset() == offset);
        };
        assert_eq!(editor.vim_status().unwrap().mode, VimMode::Normal);

        // `%` on `\begin{proof}` and back.
        let begin = offset_of(&editor, "\\begin{proof}", 2);
        editor.place_cursor_at(begin);
        browser.key(gdk::Key::percent, 0);
        wait_cursor(offset_of(&editor, "\\end{proof}", 0));
        // The highlight is there.
        common::pump_until(TIMEOUT, || {
            tagged_texts(&editor, MATCH_TAG) == ["\\begin{proof}", "\\end{proof}"]
        });
        browser.key(gdk::Key::percent, 0);
        wait_cursor(begin - 2);
        // On `{` of `\frac{a}`.
        editor.place_cursor_at(offset_of(&editor, "{a}", 0));
        browser.key(gdk::Key::percent, 0);
        wait_cursor(offset_of(&editor, "}{b}", 0));
        assert_eq!(editor.document().text(), text, "nothing typed");

        // `d%` on `(x)`: Vim's own `%` as the motion of `d` (GtkSourceView 5.20's is
        // exclusive: it leaves the `)`).
        editor.place_cursor_at(offset_of(&editor, "(x)", 0));
        browser.text("d");
        browser.key(gdk::Key::percent, 0);
        common::pump_until(TIMEOUT, || !editor.document().text().contains("(x"));
        let after = editor.document().text();
        assert!(
            after.contains("and ") && after.contains(" here."),
            "{after:?}"
        );
        assert_eq!(editor.vim_status().unwrap().mode, VimMode::Normal);
        browser.text("u");
        common::pump_until(TIMEOUT, || editor.document().text() == text);

        // Ctrl+% (the accelerator) with Vim off; `%` types a percent sign then.
        f.state.set_vim_enabled(false);
        view.grab_focus();
        common::pump_until(TIMEOUT, || view.has_focus());
        let open = offset_of(&editor, "\\frac{a}", 5);
        editor.place_cursor_at(open);
        // Broadway's keymap consumes no modifier: `%` with Ctrl only.
        browser.key(gdk::Key::percent, CONTROL);
        wait_cursor(open + 2);
        browser.key(gdk::Key::percent, 0);
        common::pump_until(TIMEOUT, || editor.document().text().contains("{a%}"));
    }
}

// ----- performance -----

/// Cursor moves over `large.tex` (60k lines): plain text, every kind of delimiter, and the
/// environments that span the document. Budget (ARCHITECTURE §7): ≤ 0.5 ms per
/// move (p99), the highlight applied, in release builds.
fn perf_cursor_moves() {
    let text = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    let f = fixture("perf", &[("large.tex", text.as_str())]);
    let editor = f.open("large.tex");
    let doc = editor.document().clone();
    // The first query builds the environment cache from the whole text (once).
    let t = Instant::now();
    let at = text.find("\\begin{document}").unwrap();
    editor.place_cursor_at(at);
    pairs::refresh_now(&editor);
    let first = t.elapsed();
    assert_eq!(tagged(&editor, MATCH_TAG).len(), 2, "document pair");
    // The positions: every delimiter of a few hundred lines in the middle plus plain text, and
    // the far environment ends.
    let index = lemma_core::LineIndex::new(&text);
    let mut positions = Vec::new();
    let from = index.line_start(30_000);
    let to = index.line_start(30_400);
    for (i, b) in text[from..to].bytes().enumerate() {
        if matches!(
            b,
            b'\\' | b'{' | b'}' | b'$' | b'(' | b')' | b'[' | b']' | b'a'
        ) {
            positions.push(from + i);
        }
    }
    positions.push(text.find("\\begin{document}").unwrap());
    positions.push(text.rfind("\\end{document}").unwrap());
    // The cost of this module per move: the update a move queues (the move itself, GTK's and
    // the other modules' work on it, is timed apart).
    let mut costs = Vec::with_capacity(positions.len());
    let mut moves = Vec::with_capacity(positions.len());
    for &offset in &positions {
        let t = Instant::now();
        editor.place_cursor_at(offset);
        moves.push(t.elapsed());
        let t = Instant::now();
        pairs::refresh_now(&editor);
        costs.push(t.elapsed());
    }
    // Typing next to an environment (its line is read again).
    let line = 30_200;
    let at = doc.line_index().line_start(line);
    for _ in 0..50 {
        doc.apply_edits(&[TextEdit::insert(at, "\\begin{x}\\end{x} ")], Origin::User)
            .unwrap();
        editor.place_cursor_at(at);
        let t = Instant::now();
        pairs::refresh_now(&editor);
        costs.push(t.elapsed());
    }
    // The worst search: an unmatched brace, whose partner is looked for over `MAX_SCAN_LINES`
    // lines (nearly all with braces) before the search gives up, alternating with plain text.
    let at = doc.line_index().line_start(30_300);
    doc.apply_edits(&[TextEdit::insert(at, "{ ")], Origin::User)
        .unwrap();
    let plain = at + 2;
    let mut worst = Vec::new();
    for i in 0..60 {
        editor.place_cursor_at(if i % 2 == 0 { at } else { plain });
        let t = Instant::now();
        pairs::refresh_now(&editor);
        worst.push(t.elapsed());
    }
    editor.place_cursor_at(at);
    pairs::refresh_now(&editor);
    assert_eq!(pairs::highlighted(&editor), None, "the search gave up");
    costs.extend(worst.iter().copied());
    let stats = |v: &mut Vec<Duration>| {
        v.sort();
        let q = |p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)];
        (q(0.5), q(0.99), *v.last().unwrap())
    };
    let (p50, p99, max) = stats(&mut costs);
    let (m50, m99, _) = stats(&mut moves);
    let (_, _, worst_max) = stats(&mut worst);
    println!(
        "    pairs per cursor move on large.tex ({} moves): p50 {p50:?}, p99 {p99:?}, max {max:?}; \
         first query (builds the environment cache) {first:?}; the move itself (GTK and the \
         other modules) p50 {m50:?}, p99 {m99:?}; an unmatched brace (the longest search) \
         {worst_max:?}",
        costs.len()
    );
    // A lone `\r` near the top (GTK's lines then differ from the document's): offsets go
    // through one copy of the text per version, so a move costs a pass over the text at most.
    let buffer = editor.buffer().clone();
    let mut iter = buffer.iter_at_line_offset(5, 2).unwrap();
    buffer.insert(&mut iter, "\r");
    assert_ne!(buffer.line_count() as u32, doc.line_index().line_count());
    let copy = (0..5)
        .map(|_| {
            let t = Instant::now();
            let _ = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
            t.elapsed()
        })
        .min()
        .unwrap();
    let mut lone = Vec::new();
    for (i, &offset) in positions.iter().enumerate().step_by(16) {
        editor.place_cursor_at(offset + 1);
        let t = Instant::now();
        pairs::refresh_now(&editor);
        lone.push(t.elapsed());
        if i == 0 {
            // The copy of this version is made.
            lone.clear();
        }
    }
    let (l50, l99, _) = stats(&mut lone);
    println!(
        "    with a lone CR ({} moves): p50 {l50:?}, p99 {l99:?}; one copy of the text {copy:?}",
        lone.len()
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release builds only)");
        return;
    }
    assert!(
        p99 <= Duration::from_micros(500),
        "p99 {p99:?} over the 0.5 ms budget"
    );
    assert!(
        worst_max <= Duration::from_millis(2),
        "an unmatched brace takes {worst_max:?}"
    );
    assert!(
        l99 <= copy * 2 + Duration::from_millis(2),
        "with a lone CR a move takes {l99:?} (one copy of the text: {copy:?})"
    );
}
