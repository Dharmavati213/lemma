//! Regression tests for the defects confirmed in the second review round, area E1
//! (highlighting, spell check, typing helpers). Each test reproduces one finding and fails until
//! it is fixed.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress2_e1`.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::editor::highlight::{self, Highlighter, NO_SPELL_TAG};
use lemma_app::editor::keys;
use lemma_app::editor::{Editor, VimMode, completion, spell, typing, vim};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    common::run(&[
        // E1-2
        (
            "regress2_closer_types_over_foreign_brace",
            closer_types_over_foreign_brace,
        ),
        (
            "regress2_closer_after_multiline_group_opener",
            closer_after_multiline_group_opener,
        ),
        // E1-3
        (
            "regress2_backspace_deletes_foreign_closer",
            backspace_deletes_foreign_closer,
        ),
        // E1-5
        (
            "regress2_enter_and_begin_cost_scales_with_document",
            enter_and_begin_cost,
        ),
        // E1-6
        ("regress2_lone_cr_tagging_cost", lone_cr_tagging_cost),
        // E1-7
        (
            "regress2_lone_cr_margin_idle_makes_progress",
            lone_cr_margin_progress,
        ),
        // E1-9
        (
            "regress2_changed_word_list_reread_on_reload",
            word_list_reread_on_reload,
        ),
        // E1-4
        (
            "regress2_option_brackets_are_no_spell",
            option_brackets_no_spell,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(30);
/// The cursor in the tables.
const CURSOR: char = '‸';
/// Backspace in the typed keys.
const BS: char = '⌫';

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ───────────── Fixture ─────────────

struct Fixture {
    state: Rc<WindowState>,
    editor: Rc<Editor>,
    _dir: PathBuf,
}

fn base_config() -> Config {
    let mut config = Config::default();
    config.build.on_save = false;
    config.build.on_agent_turn_end = false;
    config.editor.vim = false;
    config.completion.auto = false;
    config.spell.word_lists = Vec::new();
    config.spell.enabled = false;
    config.agent.node = "/nonexistent/lemma-test/node".into();
    config.agent.claude = "/nonexistent/lemma-test/claude".into();
    config
}

fn window(name: &str, files: &[(&str, &str)], mut config: Config) -> (Rc<WindowState>, PathBuf) {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    for (file, text) in files {
        std::fs::write(dir.join(file), text).unwrap();
    }
    config.agent.adapter_dir = dir.join(".no-adapter");
    let app = common::application();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, true);
    lemma_app::app::set_accels(&app, &state.config.keys);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    (state, dir)
}

/// Opens `path` without `block_on` (which never returns while an idle spins, see E1-7).
fn open(state: &Rc<WindowState>, path: &Path) -> Rc<Editor> {
    let slot: Rc<RefCell<Option<Rc<Editor>>>> = Rc::default();
    let out = slot.clone();
    let st = state.clone();
    let path = path.to_owned();
    glib::MainContext::default().spawn_local(async move {
        let editor = st.open_file(&path).await.expect("open");
        out.replace(Some(editor));
    });
    common::pump_until(Duration::from_secs(120), || slot.borrow().is_some());
    slot.take().unwrap()
}

fn fixture(name: &str, file: &str, text: &str, config: Config) -> Fixture {
    let (state, dir) = window(name, &[(file, text)], config);
    let editor = open(&state, &dir.join(file));
    common::pump_until(TIMEOUT, || editor.view().has_focus());
    completion::refresh_project(&state);
    common::pump_until(TIMEOUT, || !completion::project_busy(&state));
    Fixture {
        state,
        editor,
        _dir: dir,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        completion::hide(&self.state, &self.editor);
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

impl Fixture {
    fn buffer(&self) -> &sourceview5::Buffer {
        self.editor.buffer()
    }

    /// The text with [`CURSOR`] at the cursor.
    fn shown(&self) -> String {
        let buffer = self.buffer();
        let cursor = buffer.iter_at_mark(&buffer.get_insert());
        let before = buffer.text(&buffer.start_iter(), &cursor, true);
        let after = buffer.text(&cursor, &buffer.end_iter(), true);
        format!("{before}{CURSOR}{after}")
    }

    /// Puts `marked` (with [`CURSOR`]) in the buffer, the cursor at the mark.
    fn set(&self, marked: &str) {
        let offset = marked.chars().position(|c| c == CURSOR).expect("a cursor");
        let text: String = marked.chars().filter(|&c| c != CURSOR).collect();
        let buffer = self.buffer();
        buffer.set_text(&text);
        let iter = buffer.iter_at_offset(i32::try_from(offset).unwrap());
        buffer.place_cursor(&iter);
        common::pump_until(TIMEOUT, || self.editor.document().has_line_states());
        common::pump_for(Duration::from_millis(5));
    }

    fn mode(&self) -> Option<VimMode> {
        self.editor.vim_status().map(|s| s.mode)
    }

    fn wait(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            common::pump_for(Duration::from_millis(5));
        }
    }

    /// What GTK does with a key (as `gtk_typing`'s `Emulated`): the key layer, then Vim, then
    /// the text view's own insertion.
    fn key(&self, keyval: gdk::Key, state: gdk::ModifierType) {
        let event = keys::event_for(&self.editor, keyval, state);
        if keys::dispatch(self.editor.view(), &event) == glib::Propagation::Proceed
            && !self.editor.vim_blocks_key(keyval, state)
        {
            let consumed = self
                .editor
                .vim()
                .is_some_and(|im| vim::feed_key(&im, self.editor.view(), keyval, state));
            if !consumed {
                let buffer = self.buffer();
                match keyval {
                    gdk::Key::Return => {
                        buffer.insert_interactive_at_cursor("\n", true);
                    }
                    gdk::Key::BackSpace => {
                        let mut cursor = buffer.iter_at_mark(&buffer.get_insert());
                        buffer.backspace(&mut cursor, true, true);
                    }
                    gdk::Key::Escape => {}
                    k => {
                        if let Some(c) = k.to_unicode().filter(|c| !c.is_control()) {
                            buffer.insert_interactive_at_cursor(&c.to_string(), true);
                        }
                    }
                }
            }
        }
        common::pump_for(Duration::from_millis(2));
    }

    /// Types `text` (`\n` = Return, `⌫` = Backspace).
    fn type_text(&self, text: &str) {
        for c in text.chars() {
            let key = match c {
                '\n' => gdk::Key::Return,
                BS => gdk::Key::BackSpace,
                c => keyval(c),
            };
            let state = if c.is_uppercase() {
                gdk::ModifierType::SHIFT_MASK
            } else {
                gdk::ModifierType::empty()
            };
            self.key(key, state);
        }
    }

    /// Puts `marked` in the buffer and starts typing at its cursor (in Vim: `i`).
    fn start(&self, marked: &str) {
        if self.editor.vim().is_some() {
            self.key(gdk::Key::Escape, gdk::ModifierType::empty());
            self.wait("normal mode", || self.mode() == Some(VimMode::Normal));
        }
        self.set(marked);
        if self.editor.vim().is_some() {
            self.key(keyval('i'), gdk::ModifierType::empty());
            self.wait("insert mode", || self.mode() == Some(VimMode::Insert));
        }
    }
}

fn keyval(c: char) -> gdk::Key {
    gdk::unicode_to_keyval(u32::from(c))
        .to_value()
        .get::<gdk::Key>()
        .expect("a keyval")
}

/// Types each row (`doc`, typed keys, expected text) in plain editing and in Vim insert mode,
/// and returns the rows that came out wrong.
fn run_rows(name: &str, rows: &[(&str, &str, &str)]) -> Vec<String> {
    let mut wrong = Vec::new();
    for vim in [false, true] {
        let mut config = base_config();
        config.editor.vim = vim;
        let f = fixture(&format!("{name}-{vim}"), "main.tex", "\n", config);
        for (doc, typed, expected) in rows {
            f.start(doc);
            f.type_text(typed);
            common::pump_for(Duration::from_millis(5));
            let shown = f.shown();
            if shown != *expected {
                let label = if vim { "vim insert" } else { "plain" };
                wrong.push(format!(
                    "{label}: typing {typed:?} into {doc:?} gives {shown:?}, expected {expected:?}"
                ));
            }
        }
    }
    wrong
}

// ───────────── E1-2: closers typed over by a per-line brace count ─────────────

/// A `}` typed on the line holding a multi-line group's closing `}` types over that `}`: the
/// group loses its closer.
fn closer_types_over_foreign_brace() {
    let rows: &[(&str, &str, &str)] = &[
        (
            "Text\\footnote{As shown\nin‸}\nNext.\n",
            " \\cite{X}",
            "Text\\footnote{As shown\nin \\cite{X}‸}\nNext.\n",
        ),
        (
            "\\section{A long title\non ‸}\n",
            "\\emph{x}",
            "\\section{A long title\non \\emph{x}‸}\n",
        ),
    ];
    let wrong = run_rows("e1-2a", rows);
    assert!(
        wrong.is_empty(),
        "the typed `}}` replaced a closer the helper never inserted:\n{}",
        wrong.join("\n")
    );
}

/// On the line that opens a multi-line group, the `}` typed after a paired `{` does not type
/// over the `}` the helper inserted: one `}` too many.
fn closer_after_multiline_group_opener() {
    let rows: &[(&str, &str, &str)] = &[
        (
            "Text\\footnote{Long note ‸\n}\nNext.\n",
            "\\cite{a}",
            "Text\\footnote{Long note \\cite{a}‸\n}\nNext.\n",
        ),
        (
            "\\section{A long title on ‸\ntwo lines}\n",
            "\\emph{x}",
            "\\section{A long title on \\emph{x}‸\ntwo lines}\n",
        ),
        (
            "$$\\bigl( a + ‸\n b \\bigr)$$\n",
            "f(y)",
            "$$\\bigl( a + f(y)‸\n b \\bigr)$$\n",
        ),
    ];
    let wrong = run_rows("e1-2b", rows);
    assert!(
        wrong.is_empty(),
        "typing a balanced group left an extra closer:\n{}",
        wrong.join("\n")
    );
}

// ───────────── E1-3: Backspace deletes a closer the helper never inserted ─────────────

fn backspace_deletes_foreign_closer() {
    let rows: &[(&str, &str, &str)] = &[
        // `$` before `$x$` is not paired; Backspace must only take the typed `$` back.
        ("Let ‸$x$ be.\n", "$⌫", "Let ‸$x$ be.\n"),
        // `{` on a line with an unmatched `}` is not paired; Backspace must keep that `}`.
        (
            "Text\\footnote{As shown\nin ‸}\nNext.\n",
            "{⌫",
            "Text\\footnote{As shown\nin ‸}\nNext.\n",
        ),
    ];
    let wrong = run_rows("e1-3", rows);
    assert!(
        wrong.is_empty(),
        "a typo and Backspace deleted text the user did not type:\n{}",
        wrong.join("\n")
    );
}

// ───────────── E1-5: Enter and `}` copy and scan the whole document ─────────────

/// A document of `n` prose lines in a `document` environment.
fn paper(n: usize) -> String {
    let mut text = String::from("\\documentclass{article}\n\\begin{document}\n");
    for i in 0..n {
        text.push_str(&format!(
            "Line {i} of the paper, with some words and $x_{{{i}}} + y$ math.\n"
        ));
    }
    text.push_str("\\end{document}\n");
    text
}

/// Median and p90 of `samples`.
fn stats(samples: &mut [Duration]) -> (Duration, Duration) {
    samples.sort();
    let at = |q: f64| samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)];
    (at(0.5), at(0.9))
}

/// The typing helper's own main-thread time for its keys on the fixture's document: the time
/// of the key in the key layer minus the time of the same edits made straight in the buffer
/// (which the scanner, the highlighter and GTK cost anyway). The keys: `}` completing
/// `\begin{proof` near the top, Enter after `\begin{proof}` near the top, and Enter at the end
/// of a prose line in the middle (no helper applies, nothing is edited).
fn key_costs(f: &Fixture, rounds: usize) -> [(Duration, Duration); 3] {
    let buffer = f.buffer().clone();
    let doc = f.editor.document().clone();
    let view = f.editor.view().clone();
    let dispatch = |keyval: gdk::Key| {
        let event = keys::event_for(&f.editor, keyval, gdk::ModifierType::empty());
        let t = Instant::now();
        let propagation = keys::dispatch(&view, &event);
        (t.elapsed(), propagation)
    };
    let settle = || {
        common::pump_until(TIMEOUT, || doc.has_line_states());
        common::pump_for(Duration::from_millis(3));
    };
    // Puts `text` as line `line` and the cursor at its end.
    let put_line = |line: i32, text: &str| {
        let mut at = buffer.iter_at_line(line).unwrap();
        buffer.insert(&mut at, &format!("{text}\n"));
        let mut cursor = buffer.iter_at_line(line).unwrap();
        cursor.forward_to_line_end();
        buffer.place_cursor(&cursor);
        settle();
    };
    let drop_lines = |line: i32, n: i32| {
        let mut start = buffer.iter_at_line(line).unwrap();
        let mut end = buffer.iter_at_line(line + n).unwrap();
        buffer.delete(&mut start, &mut end);
        settle();
    };
    // The same insertions at the cursor (in this order, as the helper makes them), straight in
    // the buffer, as one user action.
    let direct = |texts: &[&str]| {
        let t = Instant::now();
        buffer.begin_user_action();
        let offset = buffer.cursor_position();
        for text in texts {
            buffer.insert(&mut buffer.iter_at_offset(offset), text);
        }
        buffer.place_cursor(&buffer.iter_at_offset(offset + 1));
        buffer.end_user_action();
        t.elapsed()
    };
    let overhead = |key: Duration, edit: Duration| key.saturating_sub(edit);
    let mut close = Vec::new();
    let mut enter_begin = Vec::new();
    let mut enter_prose = Vec::new();
    let line = 4;
    for _ in 0..rounds {
        // `}` completing `\begin{proof` on a line of its own near the top.
        put_line(line, "\\begin{proof");
        let (key, p) = dispatch(gdk::Key::braceright);
        assert_eq!(p, glib::Propagation::Stop, "the helper handles `}}`");
        let added = doc.line_text(line as u32 + 1).unwrap_or_default();
        assert!(added.starts_with("\\end{proof}"), "{added:?}");
        drop_lines(line, 2);
        put_line(line, "\\begin{proof");
        // The helper's two edits: `\end{proof}` at the line end, then the `}` before it.
        let edit = direct(&["\n\\end{proof}", "}"]);
        close.push(overhead(key, edit));
        drop_lines(line, 2);

        // Enter after `\begin{proof}`.
        put_line(line, "\\begin{proof}");
        let (key, p) = dispatch(gdk::Key::Return);
        assert_eq!(p, glib::Propagation::Stop, "the helper handles Enter");
        let body = doc.line_text(line as u32 + 1).unwrap_or_default();
        let body = body.trim_end_matches('\n').to_owned();
        drop_lines(line, 3);
        put_line(line, "\\begin{proof}");
        let edit = direct(&[&format!("\n{body}\n\\end{{proof}}")]);
        enter_begin.push(overhead(key, edit));
        drop_lines(line, 3);

        // Enter at the end of a prose line in the middle: no helper applies.
        let middle = buffer.line_count() / 2;
        let mut cursor = buffer.iter_at_line(middle).unwrap();
        cursor.forward_to_line_end();
        buffer.place_cursor(&cursor);
        settle();
        let (key, p) = dispatch(gdk::Key::Return);
        assert_eq!(
            p,
            glib::Propagation::Proceed,
            "no helper for Enter in prose"
        );
        enter_prose.push(key);
    }
    [
        stats(&mut close),
        stats(&mut enter_begin),
        stats(&mut enter_prose),
    ]
}

fn enter_and_begin_cost() {
    let rounds = 21;
    let small = paper(500);
    let large = paper(9_990);
    let f = fixture("e1-5-small", "main.tex", &small, base_config());
    let small_costs = key_costs(&f, rounds);
    drop(f);
    let f = fixture("e1-5-large", "main.tex", &large, base_config());
    let large_costs = key_costs(&f, rounds);
    // For the record: Enter in prose with the typing helpers detached.
    typing::detach(&f.state, &f.editor);
    let detached = {
        let mut samples: Vec<Duration> = (0..rounds)
            .map(|_| {
                let event =
                    keys::event_for(&f.editor, gdk::Key::Return, gdk::ModifierType::empty());
                let t = Instant::now();
                let _ = keys::dispatch(f.editor.view(), &event);
                t.elapsed()
            })
            .collect();
        stats(&mut samples)
    };
    println!("    Enter in prose on 9992 lines, typing helpers detached: {detached:?}");
    let names = [
        "`}` completing \\begin{proof}",
        "Enter after \\begin{proof}",
        "Enter in prose",
    ];
    let mut wrong = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let (s50, s90) = small_costs[i];
        let (l50, l90) = large_costs[i];
        println!(
            "    {name}, the helper's own time: 502 lines median {s50:?} p90 {s90:?}; 9992 lines \
             median {l50:?} p90 {l90:?}"
        );
        // Work that does not depend on the document's size: 20 times the lines may cost a
        // little more, not 20 times as much.
        if l50 > s50 * 3 + Duration::from_micros(300) {
            wrong.push(format!(
                "{name}: median {l50:?} on 9992 lines against {s50:?} on 502 lines"
            ));
        }
        // §7: ≤ 2 ms p99 per keystroke at ≤ 10k lines (checked in release builds).
        if !cfg!(debug_assertions) && l90 > Duration::from_millis(2) {
            wrong.push(format!(
                "{name}: p90 {l90:?} on 9992 lines, above the §7 budget of 2 ms"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the typing helpers' work per key grows with the document:\n{}",
        wrong.join("\n")
    );
}

// ───────────── E1-6 / E1-7: a lone CR makes tagging O(file size) per range ─────────────

fn hl(editor: &Editor) -> Rc<Highlighter> {
    highlight::highlighter(editor).expect("the editor is highlighted")
}

/// Whether every line of the window is tagged with its current tokens.
fn window_tagged(h: &Highlighter) -> bool {
    h.window()
        .is_some_and(|w| w.tagged.clone().all(|line| h.is_current(line)))
}

/// Pumps the main loop until the window is tagged; returns the highlighter's work meanwhile, or
/// `None` after `timeout`.
fn settle_tagging(h: &Highlighter, timeout: Duration) -> Option<Duration> {
    let work = h.work_time();
    let deadline = Instant::now() + timeout;
    let context = glib::MainContext::default();
    // Let the scroll's idles get queued first.
    context.iteration(false);
    while !window_tagged(h) {
        if Instant::now() >= deadline {
            return None;
        }
        context.iteration(false);
    }
    // And whatever is still queued.
    while context.pending() && Instant::now() < deadline {
        context.iteration(false);
    }
    Some(h.work_time() - work)
}

/// Scrolls `editor` so that line `line` is at the top.
fn scroll_to(editor: &Editor, line: u32) {
    let doc = editor.document();
    let iter = doc.iter_at_byte(doc.line_index().line_start(line)).unwrap();
    let (y, _) = editor.view().line_yrange(&iter);
    editor.view().vadjustment().unwrap().set_value(f64::from(y));
}

fn large_tex() -> String {
    std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap()
}

fn lone_cr_tagging_cost() {
    // 20k lines (2.3 MB), each with many token kinds (so many tags per line), in a small font
    // without wrapping (so many lines on screen).
    let mut text = String::from("\\documentclass{article}\n\\begin{document}\n");
    for i in 0..20_000 {
        text.push_str(&format!(
            "\\section{{T}} \\begin{{x}}\\end{{x}} A~\\ref{{a{i}}} \\cite{{b}} \\label{{c}} \
             \\input{{d}} \\usepackage{{e}} \\hypersetup{{k=v}} \\verb|v| \\emph{{w}} \
             $x^2 \\alpha$ % c\n"
        ));
    }
    text.push_str("\\end{document}\n");
    let mut config = base_config();
    config.editor.font = "Monospace 4".into();
    config.editor.wrap = false;
    let (state, dir) = window("e1-6", &[("main.tex", &text)], config);
    lemma_app::editor::install_css(&gdk::Display::default().unwrap(), &state.config.editor);
    let editor = open(&state, &dir.join("main.tex"));
    common::pump_until(TIMEOUT * 4, || {
        editor.view().height() > 0 && editor.document().has_line_states()
    });
    let h = hl(&editor);
    settle_tagging(&h, TIMEOUT).expect("the first window is tagged");
    let lines = editor.document().line_index().line_count();
    // The pass over the visible lines after a scroll (before the next frame, no time budget).
    let visible_pass = |line: u32| {
        scroll_to(&editor, line);
        let work = h.work_time();
        h.flush_visible();
        let visible = h.window().map(|w| w.visible).unwrap_or_default();
        (h.work_time() - work, visible)
    };
    // Without a lone CR: a scroll to untagged lines near the end.
    let (plain, plain_lines) = visible_pass(lines * 85 / 100);
    settle_tagging(&h, TIMEOUT).expect("tagged");
    // One lone CR near the top (an agent edit or a paste can bring one in).
    let buffer = editor.buffer();
    let mut at = buffer.iter_at_line_offset(5, 2).unwrap();
    buffer.insert(&mut at, "\r");
    assert_ne!(
        buffer.line_count() as u32,
        editor.document().line_index().line_count()
    );
    h.flush_visible();
    common::pump_for(Duration::from_millis(200));
    // The cost of one copy of the text, for scale.
    let copy = (0..5)
        .map(|_| {
            let t = Instant::now();
            let _ = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
            t.elapsed()
        })
        .min()
        .unwrap();
    let (with_cr, cr_lines) = visible_pass(lines * 95 / 100);
    state.window.destroy();
    common::pump_for(Duration::from_millis(20));
    println!(
        "    visible pass after a scroll near the end of a 30k-line document: plain {plain:?} \
         (lines {plain_lines:?}); after one lone CR {with_cr:?} (lines {cr_lines:?}); one copy \
         of the text {copy:?}"
    );
    // A few copies of the text per pass are acceptable (the document's own byte offsets cost
    // one each in this mode); a conversion from the start of the text for every tag range of
    // every line is not.
    assert!(
        with_cr < plain * 3 + copy * 6 + Duration::from_millis(5),
        "after one lone CR, the visible pass after a scroll took {with_cr:?} instead of \
         {plain:?} (one copy of the text: {copy:?})"
    );
}

fn lone_cr_margin_progress() {
    // A 9 MB file (large.tex four times) with one lone CR near the top.
    let large = large_tex();
    let mut text = String::with_capacity(large.len() * 4 + 8);
    for _ in 0..4 {
        text.push_str(&large);
    }
    let at = text.match_indices('\n').nth(9).unwrap().0;
    text.insert_str(at, " x\ry");
    let (state, dir) = window("e1-7", &[("big.tex", &text)], base_config());
    drop(text);
    let editor = open(&state, &dir.join("big.tex"));
    common::pump_until(Duration::from_secs(300), || {
        editor.view().height() > 0 && editor.document().has_line_states()
    });
    let h = hl(&editor);
    let buffer = editor.buffer().clone();
    assert_ne!(
        buffer.line_count() as u32,
        editor.document().line_index().line_count(),
        "GTK counts the lone CR as a line break"
    );
    let copy = (0..5)
        .map(|_| {
            let t = Instant::now();
            let _ = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
            t.elapsed()
        })
        .min()
        .unwrap();
    // The main loop runs for a while: the window should get tagged, and the background idle
    // should then stop.
    let settled = settle_tagging(&h, Duration::from_secs(20));
    let work = h.work_time();
    common::pump_for(Duration::from_secs(2));
    let idle_work = h.work_time() - work;
    let window = h.window();
    let untagged = window.as_ref().map_or(0, |w| {
        w.tagged.clone().filter(|&l| !h.is_current(l)).count()
    });
    state.window.destroy();
    common::pump_for(Duration::from_millis(20));
    println!(
        "    9 MB file with one lone CR (one copy of the text: {copy:?}): window {window:?}, \
         {untagged} lines untagged after 20 s, highlighter work in 2 more seconds {idle_work:?}"
    );
    assert!(
        settled.is_some() && untagged == 0,
        "the background idle tagged nothing in 20 s ({untagged} window lines untagged)"
    );
}

// ───────────── E1-9: changed word lists are not read again on a reload ─────────────

fn word_list_reread_on_reload() {
    let (state, dir) = window(
        "e1-9",
        &[("main.tex", "Qqwalpha and Qqwbeta here.\n")],
        base_config(),
    );
    let list = dir.join("words.txt");
    std::fs::write(&list, "Qqwalpha\n").unwrap();
    let mut config = (*state.config.get()).clone();
    config.spell.enabled = true;
    config.spell.language = "en_US".into();
    config.spell.word_lists = vec![list.clone()];
    state.set_config(Rc::new(config));
    let editor = block_on(state.open_file(&dir.join("main.tex"))).expect("open");
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.end_iter());
    let flagged = || {
        let mut words = spell::misspelled(&editor);
        words.sort();
        words
    };
    common::pump_until(TIMEOUT, || {
        spell::is_checking(&editor) && flagged() == ["Qqwbeta"]
    });
    // The user adds the word to the list (in Neovim), then changes a setting: the config is
    // reloaded.
    std::fs::write(&list, "Qqwalpha\nQqwbeta\n").unwrap();
    let mut config = (*state.config.get()).clone();
    config.editor.math_tint = config.editor.math_tint.wrapping_add(1) % 100;
    state.set_config(Rc::new(config));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !flagged().is_empty() {
        common::pump_for(Duration::from_millis(20));
    }
    let after_reload = flagged();
    // A second tab opened now reads the changed list.
    std::fs::write(dir.join("other.tex"), "Qqwbeta again.\n").unwrap();
    let other = block_on(state.open_file(&dir.join("other.tex"))).expect("open");
    other.buffer().place_cursor(&other.buffer().end_iter());
    common::pump_until(TIMEOUT, || spell::is_checking(&other));
    common::pump_for(Duration::from_millis(500));
    let in_other = spell::misspelled(&other);
    let in_first = flagged();
    state.window.destroy();
    common::pump_for(Duration::from_millis(20));
    println!(
        "    after the reload: {after_reload:?}; a tab opened later: {in_other:?}, the first \
         tab then: {in_first:?}"
    );
    assert!(
        after_reload.is_empty(),
        "a word added to a word list is still flagged after a config reload: {after_reload:?}"
    );
    assert!(
        in_first.is_empty(),
        "two tabs disagree: the new tab accepts the word, the first still flags {in_first:?}"
    );
}

// ───────────── E1-4: option brackets on visible lines are spell-checked ─────────────

fn option_brackets_no_spell() {
    let text = "\\documentclass{article}\n\
\\usepackage{amsthm,tikz-cd,hyperref}\n\
\\newtheorem{thm}{Theorem}\n\
\\newtheorem{lem}[thm]{Lemma}\n\
\\begin{document}\n\
See \\hyperref[sec:intro]{the introduction}.\n\
\\begin{center}\n\
\\begin{tikzcd}\n\
A \\arrow[rr, \"f\"] \\arrow[dl, hook] & & B \\arrow[uu, Rightarrow]\n\
\\end{tikzcd}\n\
\\end{center}\n\
\\end{document}\n";
    let (state, dir) = window("e1-4", &[("main.tex", text)], base_config());
    let editor = open(&state, &dir.join("main.tex"));
    common::pump_until(TIMEOUT, || {
        editor.view().height() > 0 && editor.document().has_line_states()
    });
    let h = hl(&editor);
    h.flush();
    let count = editor.document().line_index().line_count();
    assert!((0..count).all(|l| h.is_current(l)), "every line is tagged");
    let buffer = editor.buffer();
    let tag = buffer.tag_table().lookup(NO_SPELL_TAG).expect("the tag");
    let full = buffer.text(&buffer.start_iter(), &buffer.end_iter(), true);
    // Words inside option brackets (a shared theorem counter, a hyperref label, tikz-cd arrow
    // options) that the spell checker sees as prose.
    let mut checked = Vec::new();
    // (context, word in it)
    for (context, word) in [
        ("[thm]", "thm"),
        ("[sec:intro]", "sec:intro"),
        ("[rr,", "rr"),
        ("[dl,", "dl"),
        ("hook]", "hook"),
        ("[uu,", "uu"),
        ("Rightarrow]", "Rightarrow"),
    ] {
        let byte = full.find(context).expect("the context") + context.find(word).unwrap();
        let start = full[..byte].chars().count();
        let len = word.chars().count();
        let spell_checked = (start..start + len).any(|c| {
            !buffer
                .iter_at_offset(i32::try_from(c).unwrap())
                .has_tag(&tag)
        });
        if spell_checked {
            checked.push(word);
        }
    }
    state.window.destroy();
    common::pump_for(Duration::from_millis(20));
    assert!(
        checked.is_empty(),
        "option text without the no-spell tag on a tagged line: {checked:?}"
    );
}
