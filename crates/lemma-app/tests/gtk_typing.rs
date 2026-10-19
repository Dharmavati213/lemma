//! Typing helpers (WP2.9b acceptance; ARCHITECTURE §5 "Pairing").
//!
//! - **In process**, in plain editing and in Vim insert mode: every rule through the key layer
//!   (`keys::dispatch`, then Vim, then the text view's insertion, as in `gtk_autosnippets`), the
//!   cases that must not fire (after `\`, in comments and verbatim, on paste, undo and redo,
//!   agent edits, in Vim normal mode, after `detach`), each `[typing]` switch with a live config
//!   reload, and Enter while the completion popup is open. `perf_typing_keys` (with
//!   `--ignored`) measures the main-thread cost of the helper keys on a 5k-line document.
//! - **Real input** (`typing_helpers_on_real_key_events`, see `broadway_input`): pairs, `$`,
//!   Backspace, `\end`, `\item` and leaving a list, a paste, and Enter with the completion
//!   popup open, on real key events in plain editing and in Vim insert mode.
//!
//! In the tables `‸` marks the cursor, and the typed keys use `\n` for Enter and `⌫` for
//! Backspace.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_typing`.

mod broadway_input;
mod common;

use adw::prelude::*;
use broadway_input::{Browser, CONTROL, SHIFT};
use gtk::gdk;
use lemma_agent::EditorRequest;
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::{Editor, VimMode, completion, typing, vim};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::cell::Cell;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

const REAL_CHILD: &str = "real_input_typing_helpers";

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[(REAL_CHILD, real_child)]);
    }
    let mut tests: Vec<common::Test> = vec![
        (
            "typing_helpers_on_real_key_events",
            typing_helpers_on_real_key_events,
        ),
        ("rules_through_the_key_layer_plain", rules_plain),
        ("rules_through_the_key_layer_vim_insert", rules_vim),
        (
            "never_on_paste_undo_agent_edits_or_normal_mode",
            never_on_other_edits,
        ),
        ("each_typing_switch_turns_its_helper_off", switches),
        (
            "enter_while_the_completion_popup_is_open",
            enter_with_the_popup,
        ),
        ("shift_enter_is_a_plain_line_break", shift_enter),
        ("helpers_inside_an_active_snippet", inside_a_snippet),
        ("detach_removes_the_handler", detach),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_typing_keys", perf_typing_keys));
        tests.push(("perf_typing_large_tex", perf_typing_large_tex));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(10);
/// The cursor in the tables.
const CURSOR: char = '‸';
/// Backspace in the typed keys.
const BS: char = '⌫';

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ───────────── Fixture ─────────────

struct Fixture {
    state: Rc<WindowState>,
    editor: Rc<Editor>,
    dir: PathBuf,
}

fn config(vim: bool) -> Config {
    let mut config = Config::default();
    config.build.on_save = false;
    config.build.on_agent_turn_end = false;
    config.editor.vim = vim;
    // No word popups unless a test asks for them: Enter would belong to the popup.
    config.completion.auto = false;
    config.spell.word_lists = Vec::new();
    config.spell.enabled = false;
    config.agent.node = "/nonexistent/lemma-test/node".into();
    config.agent.claude = "/nonexistent/lemma-test/claude".into();
    config
}

fn fixture(name: &str, vim: bool) -> Fixture {
    fixture_with(name, config(vim))
}

fn fixture_with(name: &str, mut config: Config) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    std::fs::write(dir.join("main.tex"), "\n").unwrap();
    config.agent.adapter_dir = dir.join(".no-adapter");
    let app = common::application();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, true);
    lemma_app::app::set_accels(&app, &state.config.keys);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    let editor = block_on(state.open_file(&dir.join("main.tex"))).expect("open");
    common::pump_until(TIMEOUT, || editor.view().has_focus());
    completion::refresh_project(&state);
    common::pump_until(TIMEOUT, || !completion::project_busy(&state));
    Fixture { state, editor, dir }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // An open popup must not outlive its window (under Broadway it can keep the focus).
        completion::hide(&self.state, &self.editor);
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

impl Fixture {
    fn buffer(&self) -> &sourceview5::Buffer {
        self.editor.buffer()
    }

    fn mode(&self) -> Option<VimMode> {
        self.editor.vim_status().map(|s| s.mode)
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
        common::pump_for(Duration::from_millis(5));
    }

    fn wait(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; text {:?}",
                self.shown()
            );
            common::pump_for(Duration::from_millis(5));
        }
    }

    /// Opens the completion popup at the cursor. Under Broadway an earlier popup can keep the
    /// keyboard focus, and GtkSourceView then ignores the request: the window
    /// is presented and the view focused first.
    fn show_popup(&self) {
        self.state.window.present();
        self.editor.view().grab_focus();
        self.wait("the focus", || self.editor.view().has_focus());
        assert!(completion::show(&self.state, &self.editor), "proposals");
        self.wait("the popup", || completion::popup_visible(&self.editor));
    }

    fn event(&self, keyval: gdk::Key, state: gdk::ModifierType) -> KeyEvent {
        keys::event_for(&self.editor, keyval, state)
    }

    fn config(&self, edit: impl FnOnce(&mut Config)) {
        let mut config = (*self.state.config.get()).clone();
        edit(&mut config);
        self.state.set_config(Rc::new(config));
    }
}

// ───────────── Typing ─────────────

/// Something that types: real key events or the in-process emulation.
trait Typist {
    /// Presses `key` with GDK modifiers `state`.
    fn key(&mut self, f: &Fixture, key: gdk::Key, state: gdk::ModifierType);

    /// Types `text` (`\n` = Return, `⌫` = Backspace, `\t` = Tab).
    fn text(&mut self, f: &Fixture, text: &str) {
        for c in text.chars() {
            let key = match c {
                '\t' => gdk::Key::Tab,
                '\n' => gdk::Key::Return,
                BS => gdk::Key::BackSpace,
                c => broadway_input::keyval(c),
            };
            let state = if c.is_uppercase() {
                gdk::ModifierType::SHIFT_MASK
            } else {
                gdk::ModifierType::empty()
            };
            self.key(f, key, state);
        }
    }

    /// Back to Vim's normal mode (Escape); without Vim it just closes the popup.
    fn escape(&mut self, f: &Fixture) {
        self.key(f, gdk::Key::Escape, gdk::ModifierType::empty());
        if f.editor.vim().is_some() {
            f.wait("normal mode", || f.mode() == Some(VimMode::Normal));
        }
    }

    /// Puts `marked` in the buffer and starts typing at its cursor (in Vim: `i`).
    fn start(&mut self, f: &Fixture, marked: &str) {
        self.escape(f);
        f.set(marked);
        if f.editor.vim().is_some() {
            self.key(f, broadway_input::keyval('i'), gdk::ModifierType::empty());
            f.wait("insert mode", || f.mode() == Some(VimMode::Insert));
        }
    }
}

/// What GTK does with a key, emulated: the key layer, then Vim, then the text view (characters
/// typed, Return breaking the line with the line's indentation as auto-indent does, Backspace
/// deleting a character, Tab indenting by two spaces).
struct Emulated;

impl Typist for Emulated {
    fn key(&mut self, f: &Fixture, keyval: gdk::Key, state: gdk::ModifierType) {
        let event = f.event(keyval, state);
        if keys::dispatch(f.editor.view(), &event) == glib::Propagation::Proceed
            && !f.editor.vim_blocks_key(keyval, state)
        {
            let consumed = f
                .editor
                .vim()
                .is_some_and(|im| vim::feed_key(&im, f.editor.view(), keyval, state));
            let modified =
                state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
            if !consumed && !modified {
                view_key(f, keyval);
            }
        }
        common::pump_for(Duration::from_millis(2));
    }
}

/// The text view's own handling of `keyval` (see [`Emulated`]).
fn view_key(f: &Fixture, keyval: gdk::Key) {
    let buffer = f.buffer();
    match keyval {
        gdk::Key::Return => {
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            let mut start = cursor;
            start.set_line_offset(0);
            let line = buffer.text(&start, &cursor, true);
            let indent: String = line.chars().take_while(|c| *c == ' ').collect();
            buffer.insert_interactive_at_cursor(&format!("\n{indent}"), true);
        }
        gdk::Key::BackSpace => {
            let mut cursor = buffer.iter_at_mark(&buffer.get_insert());
            buffer.backspace(&mut cursor, true, true);
        }
        gdk::Key::Tab => {
            buffer.insert_interactive_at_cursor("  ", true);
        }
        k => {
            if let Some(c) = k.to_unicode().filter(|c| !c.is_control()) {
                buffer.insert_interactive_at_cursor(&c.to_string(), true);
            }
        }
    }
}

/// Real key events through the private Broadway daemon; each key waits until the editor's key
/// layer has seen it (a probe handler counts them).
struct Real {
    browser: Browser,
    seen: Rc<Cell<u64>>,
}

impl Real {
    fn new(f: &Fixture) -> Self {
        let seen: Rc<Cell<u64>> = Rc::default();
        let count = seen.clone();
        keys::register(f.editor.view(), i32::MAX, move |_, _| {
            count.set(count.get() + 1);
            glib::Propagation::Proceed
        })
        .expect("the view has a key layer");
        Self {
            browser: Browser::connect(),
            seen,
        }
    }
}

impl Typist for Real {
    fn key(&mut self, f: &Fixture, key: gdk::Key, state: gdk::ModifierType) {
        let mut mods = 0;
        if state.contains(gdk::ModifierType::SHIFT_MASK) {
            mods |= SHIFT;
        }
        if state.contains(gdk::ModifierType::CONTROL_MASK) {
            mods |= CONTROL;
        }
        let before = self.seen.get();
        self.browser.key(key, mods);
        f.wait("the key to arrive", || self.seen.get() > before);
        common::pump_for(Duration::from_millis(5));
    }
}

// ───────────── The rules ─────────────

/// One row: start from `doc`, type `typed`, and the text is `expected` (cursor marked).
struct Row {
    doc: &'static str,
    typed: &'static str,
    expected: &'static str,
}

const fn row(doc: &'static str, typed: &'static str, expected: &'static str) -> Row {
    Row {
        doc,
        typed,
        expected,
    }
}

const ROWS: &[Row] = &[
    // Pairs, typed over.
    row("a ‸", "{x}", "a {x}‸"),
    row("a ‸", "(x)", "a (x)‸"),
    row("a ‸", "[x]", "a [x]‸"),
    row("a ‸", "{", "a {‸}"),
    row("‸word", "(", "(‸word"),
    row("a ‸ b)", "(", "a (‸ b)"),
    // In math: `\{\}`, `\left(\right)`, half-open intervals.
    row("$x‸$", " \\{a\\}", "$x \\{a\\}‸$"),
    row("$‸$", "\\left(x)", "$\\left(x\\right)‸$"),
    row("$‸$", "[0,1)", "$[0,1)‸$"),
    row("$‸$", "(0,1]", "$(0,1]‸$"),
    // The smart `$`.
    row("Let ‸", "$x$ be", "Let $x$ be‸"),
    row("costs \\‸", "$", "costs \\$‸"),
    // Not after `\`, not in comments or verbatim.
    row("text \\‸", "{", "text \\{‸"),
    row("‸", "\\[", "\\[‸"),
    row("% note ‸", "{($", "% note {($‸"),
    row(
        "\\begin{verbatim}\n‸\n\\end{verbatim}",
        "{$",
        "\\begin{verbatim}\n{$‸\n\\end{verbatim}",
    ),
    // Backspace on an empty pair.
    row("a ‸", "{⌫", "a ‸"),
    row("a ‸", "$⌫", "a ‸"),
    row("$‸$", "\\left(⌫", "$\\left‸$"),
    row("a {x‸}", "⌫", "a {‸}"),
    // Groups and formulas spanning lines: brackets are counted over the paragraph, and a closer
    // the helper inserted is always typed over (and deleted with its opener).
    row(
        "Text\\footnote{As shown\nin‸}\nNext.",
        " \\cite{X}",
        "Text\\footnote{As shown\nin \\cite{X}‸}\nNext.",
    ),
    row(
        "\\section{A long title\non ‸}\n",
        "\\emph{x}",
        "\\section{A long title\non \\emph{x}‸}\n",
    ),
    row(
        "Text\\footnote{Long note ‸\n}\nNext.",
        "\\cite{a}",
        "Text\\footnote{Long note \\cite{a}‸\n}\nNext.",
    ),
    row(
        "$$\\bigl( a + ‸\n b \\bigr)$$",
        "f(y)",
        "$$\\bigl( a + f(y)‸\n b \\bigr)$$",
    ),
    row("$\\frac{a‸}{}$", "}", "$\\frac{a}‸{}$"),
    // A typo and Backspace take back only what was typed.
    row("Let ‸$x$ be.", "$⌫", "Let ‸$x$ be."),
    row(
        "Text\\footnote{As shown\nin ‸}\nNext.",
        "{⌫",
        "Text\\footnote{As shown\nin ‸}\nNext.",
    ),
    // `\end` on `}` and Enter after `\begin`, with an indented body.
    row("‸", "\\begin{proof}", "\\begin{proof}‸\n\\end{proof}"),
    row(
        "‸",
        "\\begin{theorem}[Main]\nx",
        "\\begin{theorem}[Main]\n  x‸\n\\end{theorem}",
    ),
    row(
        "  ‸",
        "\\begin{align}\n",
        "  \\begin{align}\n    ‸\n  \\end{align}",
    ),
    row(
        "\\begin{proof}\n‸\n\\end{proof}",
        "\\begin{align}",
        "\\begin{proof}\n\\begin{align}‸\n\\end{align}\n\\end{proof}",
    ),
    // `\item` on Enter, and leaving the list on an empty item.
    row(
        "\\begin{itemize}\n  \\item a‸\n\\end{itemize}",
        "\nb",
        "\\begin{itemize}\n  \\item a\n  \\item b‸\n\\end{itemize}",
    ),
    row(
        "‸",
        "\\begin{enumerate}\nfirst\nsecond\n\nNext",
        "\\begin{enumerate}\n  \\item first\n  \\item second\n\\end{enumerate}\nNext‸",
    ),
];

/// Runs the table with `typist`; `label` names the mode in failures.
fn run_rows(f: &Fixture, typist: &mut dyn Typist, rows: &[Row], label: &str) {
    for row in rows {
        typist.start(f, row.doc);
        typist.text(f, row.typed);
        // The completion popup's idle work and highlighting settle.
        common::pump_for(Duration::from_millis(5));
        assert_eq!(
            f.shown(),
            row.expected,
            "{label}: typing {:?} into {:?}",
            row.typed,
            row.doc
        );
    }
    typist.escape(f);
}

fn rules_plain() {
    let f = fixture("rules-plain", false);
    run_rows(&f, &mut Emulated, ROWS, "plain");
}

fn rules_vim() {
    let f = fixture("rules-vim", true);
    assert!(f.editor.vim().is_some());
    run_rows(&f, &mut Emulated, ROWS, "vim insert");
}

// ───────────── What must not fire ─────────────

/// Pastes, undo and redo, agent edits and Vim's normal mode never run a helper.
fn never_on_other_edits() {
    for vim in [false, true] {
        let label = if vim { "vim" } else { "plain" };
        let f = fixture(&format!("never-{label}"), vim);
        let mut typist = Emulated;
        // A paste of `{`, `$` and `\begin{x}`.
        f.editor.view().clipboard().set_text("{$\\begin{x}");
        common::pump_for(Duration::from_millis(20));
        typist.start(&f, "a ‸");
        f.editor.view().emit_paste_clipboard();
        f.wait("the paste", || f.shown() == "a {$\\begin{x}‸");
        common::pump_for(Duration::from_millis(20));
        assert_eq!(
            f.shown(),
            "a {$\\begin{x}‸",
            "{label}: a paste is not typing"
        );

        // Undo and redo replay what was typed, and nothing more.
        typist.start(&f, "x ‸");
        typist.text(&f, "{");
        assert_eq!(f.shown(), "x {‸}");
        typist.escape(&f);
        f.buffer().undo();
        common::pump_for(Duration::from_millis(5));
        assert_eq!(
            f.buffer()
                .text(&f.buffer().start_iter(), &f.buffer().end_iter(), true),
            "x ",
            "{label}: one undo removes the pair"
        );
        f.buffer().redo();
        common::pump_for(Duration::from_millis(5));
        assert_eq!(
            f.buffer()
                .text(&f.buffer().start_iter(), &f.buffer().end_iter(), true),
            "x {}",
            "{label}: redo brings back the pair"
        );
        // Deleting the `}` of a `\begin{x}` and undoing it adds no `\end`.
        typist.start(&f, "\\begin{proof}‸");
        typist.text(&f, "⌫");
        assert_eq!(f.shown(), "\\begin{proof‸");
        typist.escape(&f);
        f.buffer().undo();
        common::pump_for(Duration::from_millis(5));
        assert_eq!(
            f.buffer()
                .text(&f.buffer().start_iter(), &f.buffer().end_iter(), true),
            "\\begin{proof}",
            "{label}: undo is not typing"
        );

        // An agent edit that writes `\begin{x}` and `$`.
        f.set("‸\n");
        let reply = block_on(lemma_app::agent_edits::apply(
            &f.state.workspace,
            1,
            EditorRequest::Write {
                path: f.dir.join("main.tex"),
                content: "\\begin{lemma}\nLet $x\n".into(),
            },
        ));
        assert!(!reply.is_error, "{reply:?}");
        common::pump_for(Duration::from_millis(20));
        assert_eq!(f.editor.document().text(), "\\begin{lemma}\nLet $x\n");

        // Vim's normal mode: `{`, `(`, `$` are motions, not typing.
        if vim {
            typist.start(&f, "a ‸b");
            typist.escape(&f);
            for key in ['{', '$', '(', '['] {
                let event = f.event(broadway_input::keyval(key), gdk::ModifierType::empty());
                let before = f.shown();
                let _ = keys::dispatch(f.editor.view(), &event);
                assert_eq!(f.shown(), before, "normal-mode {key:?} is not typing");
            }
        }
    }
}

// ───────────── The switches ─────────────

/// Each `[typing]` key switches its helper off at once (a live config reload).
fn switches() {
    let f = fixture("switches", false);
    let mut typist = Emulated;
    let check = |typist: &mut Emulated, doc: &str, typed: &str, expected: &str, what: &str| {
        typist.start(&f, doc);
        typist.text(&f, typed);
        assert_eq!(f.shown(), expected, "{what}");
    };
    // On by default.
    check(&mut typist, "a ‸", "(", "a (‸)", "autopair on");
    f.config(|c| c.typing.autopair = false);
    check(&mut typist, "a ‸", "(", "a (‸", "autopair off");
    check(
        &mut typist,
        "a ‸",
        "{x}",
        "a {x}‸",
        "autopair off: nothing to type over",
    );
    check(&mut typist, "a {‸}", "⌫", "a ‸}", "autopair off: Backspace");
    check(&mut typist, "a ‸", "$", "a $‸$", "smart_dollar still on");
    f.config(|c| c.typing.smart_dollar = false);
    check(&mut typist, "a ‸", "$", "a $‸", "smart_dollar off");
    check(
        &mut typist,
        "‸",
        "\\begin{proof}",
        "\\begin{proof}‸\n\\end{proof}",
        "auto_end on",
    );
    f.config(|c| c.typing.auto_end = false);
    check(
        &mut typist,
        "‸",
        "\\begin{proof}",
        "\\begin{proof}‸",
        "auto_end off",
    );
    check(
        &mut typist,
        "\\begin{proof}‸\n\\end{proof}",
        "\n",
        "\\begin{proof}\n‸\n\\end{proof}",
        "auto_end off: no body indentation",
    );
    let list = "\\begin{itemize}\n  \\item a‸\n\\end{itemize}";
    check(
        &mut typist,
        list,
        "\n",
        "\\begin{itemize}\n  \\item a\n  \\item ‸\n\\end{itemize}",
        "item_on_enter on",
    );
    f.config(|c| c.typing.item_on_enter = false);
    check(
        &mut typist,
        list,
        "\n",
        "\\begin{itemize}\n  \\item a\n  ‸\n\\end{itemize}",
        "item_on_enter off",
    );
    typist.escape(&f);
}

// ───────────── Enter and the completion popup ─────────────

/// While the popup is open Enter is the popup's: accepting a proposal adds nothing; a plain
/// line break the view inserts becomes the helper's result.
fn enter_with_the_popup() {
    let mut config = config(false);
    config.completion.auto = true;
    let f = fixture_with("popup", config);
    let list = "Lorem ipsum dolor.\n\\begin{itemize}\n  \\item Lor‸\n\\end{itemize}";
    f.set(list);
    completion::refresh_project(&f.state);
    common::pump_until(TIMEOUT, || !completion::project_busy(&f.state));
    // A popup with its first row selected: Enter accepts it (the view gets no line break).
    f.show_popup();
    let event = f.event(gdk::Key::Return, gdk::ModifierType::empty());
    assert_eq!(
        keys::dispatch(f.editor.view(), &event),
        glib::Propagation::Proceed,
        "Enter goes on to the popup"
    );
    let item = completion::shown_items(&f.state, &f.editor)
        .into_iter()
        .find(|i| i.label == "Lorem")
        .expect("Lorem is proposed");
    completion::accept(&f.state, &f.editor, &item);
    completion::hide(&f.state, &f.editor);
    common::pump_for(Duration::from_millis(30));
    assert_eq!(
        f.shown(),
        "Lorem ipsum dolor.\n\\begin{itemize}\n  \\item Lorem‸\n\\end{itemize}"
    );
    // A popup without a selection lets the line break through: it gets its `\item`.
    f.set(list);
    f.show_popup();
    Emulated.key(&f, gdk::Key::Return, gdk::ModifierType::empty());
    common::pump_for(Duration::from_millis(30));
    assert_eq!(
        f.shown(),
        "Lorem ipsum dolor.\n\\begin{itemize}\n  \\item Lor\n  \\item ‸\n\\end{itemize}"
    );
    completion::hide(&f.state, &f.editor);
}

/// Inside an active snippet: pairs typed in a stop keep the session (that Tab then moves on to
/// the next stop is checked on real keys, [`REAL_ROWS`]); Backspace on an empty pair ends it
/// first (GtkSourceView would rewrite its mirrors).
fn inside_a_snippet() {
    for vim in [false, true] {
        let label = if vim { "vim" } else { "plain" };
        let f = fixture(&format!("snippet-{label}"), vim);
        let mut typist = Emulated;
        typist.start(&f, "$‸$");
        typist.text(&f, "x ff(a");
        assert_eq!(f.shown(), "$x \\frac{(a‸)}{}$", "{label}");
        assert!(completion::snippet_active(&f.state, &f.editor), "{label}");
        typist.text(&f, ")");
        assert_eq!(f.shown(), "$x \\frac{(a)‸}{}$", "{label}");
        assert!(
            completion::snippet_active(&f.state, &f.editor),
            "{label}: typing a pair keeps the session"
        );
        typist.start(&f, "$‸$");
        typist.text(&f, "x ff");
        assert!(completion::snippet_active(&f.state, &f.editor), "{label}");
        typist.text(&f, "⌫");
        assert_eq!(
            f.shown(),
            "$x \\frac‸{}$",
            "{label}: Backspace on the stop's pair"
        );
        assert!(
            !completion::snippet_active(&f.state, &f.editor),
            "{label}: the session ended"
        );
        typist.text(&f, "x");
        assert_eq!(f.shown(), "$x \\fracx‸{}$", "{label}");
        typist.escape(&f);
    }
}

/// Shift+Enter breaks the line without any helper.
fn shift_enter() {
    let f = fixture("shift-enter", false);
    f.set("\\begin{itemize}\n  \\item a‸\n\\end{itemize}");
    let event = f.event(gdk::Key::Return, gdk::ModifierType::SHIFT_MASK);
    assert_eq!(
        keys::dispatch(f.editor.view(), &event),
        glib::Propagation::Proceed
    );
    assert_eq!(f.shown(), "\\begin{itemize}\n  \\item a‸\n\\end{itemize}");
}

/// `detach` removes the key handler; detaching twice is harmless.
fn detach() {
    let f = fixture("detach", false);
    let layer = f.editor.view().key_layer().expect("a key layer");
    let handlers = layer.len();
    typing::detach(&f.state, &f.editor);
    assert_eq!(layer.len(), handlers - 1);
    let mut typist = Emulated;
    typist.start(&f, "a ‸");
    typist.text(&f, "(");
    assert_eq!(f.shown(), "a (‸");
    typing::detach(&f.state, &f.editor);
    typing::attach(&f.state, &f.editor);
    assert_eq!(layer.len(), handlers);
    typist.text(&f, "(");
    assert_eq!(f.shown(), "a ((‸)");
}

/// The keys whose rules look at environments (Enter in prose, `}` completing `\begin{proof`,
/// Enter after `\begin{proof}`) in the middle of `large.tex`: 60k lines, 10k of them with
/// `\begin`, `\end` or `\item`. Each key's whole dispatch is measured (the helper and its
/// edit).
fn perf_typing_large_tex() {
    let f = fixture("perf-large", false);
    let text = std::fs::read_to_string(common::fixtures().join("large.tex")).unwrap();
    let buffer = f.buffer().clone();
    buffer.set_text(&text);
    let doc = f.editor.document().clone();
    f.wait("the scanner states", || doc.has_line_states());
    // A prose line near the middle.
    let mut line = buffer.line_count() / 2;
    while !doc
        .line_text(line as u32)
        .is_some_and(|l| l.starts_with("Line 3 with math"))
    {
        line += 1;
    }
    let press = |key: gdk::Key| {
        let event = f.event(key, gdk::ModifierType::empty());
        let started = Instant::now();
        let propagation = keys::dispatch(f.editor.view(), &event);
        (started.elapsed(), propagation)
    };
    let settle = || {
        f.wait("the scanner states", || doc.has_line_states());
        common::pump_for(Duration::from_millis(2));
    };
    let at_line_end = |line: i32| {
        let mut iter = buffer.iter_at_line(line).unwrap();
        iter.forward_to_line_end();
        buffer.place_cursor(&iter);
        settle();
    };
    let drop_lines = |from: i32, n: i32| {
        let mut start = buffer.iter_at_line(from).unwrap();
        let mut end = buffer.iter_at_line(from + n).unwrap();
        buffer.delete(&mut start, &mut end);
        settle();
    };
    let (mut prose, mut close, mut enter) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..30 {
        // Enter at the end of a prose line: no helper applies.
        at_line_end(line);
        let (t, p) = press(gdk::Key::Return);
        assert_eq!(p, glib::Propagation::Proceed);
        prose.push(t);
        // `}` completing `\begin{proof` on a line of its own.
        let mut iter = buffer.iter_at_line(line + 1).unwrap();
        buffer.insert(&mut iter, "\\begin{proof\n");
        at_line_end(line + 1);
        let (t, p) = press(gdk::Key::braceright);
        assert_eq!(p, glib::Propagation::Stop);
        close.push(t);
        drop_lines(line + 1, 2);
        // Enter after `\begin{proof}`.
        let mut iter = buffer.iter_at_line(line + 1).unwrap();
        buffer.insert(&mut iter, "\\begin{proof}\n");
        at_line_end(line + 1);
        let (t, p) = press(gdk::Key::Return);
        assert_eq!(p, glib::Propagation::Stop);
        enter.push(t);
        drop_lines(line + 1, 3);
    }
    let mut over = Vec::new();
    for (name, samples) in [
        ("Enter in prose", &mut prose),
        ("`}` completing \\begin{proof", &mut close),
        ("Enter after \\begin{proof}", &mut enter),
    ] {
        samples.sort();
        let (p50, p90) = (samples[samples.len() / 2], samples[samples.len() * 9 / 10]);
        println!("    large.tex, {name}: median {p50:?}, p90 {p90:?}");
        // §7 asks ≤ 2 ms p99 per keystroke up to 10k lines: these keys do not grow with the
        // document, so six times that still fits.
        if !cfg!(debug_assertions) && p90 > Duration::from_millis(2) {
            over.push(format!("{name}: p90 {p90:?}"));
        }
    }
    assert!(over.is_empty(), "{}", over.join("; "));
}

// ───────────── Real input ─────────────

fn typing_helpers_on_real_key_events() {
    broadway_input::run_child(REAL_CHILD, 3);
}

/// Rows checked on real key events (a subset of [`ROWS`] covering every helper).
const REAL_ROWS: &[Row] = &[
    row("a ‸", "{x}", "a {x}‸"),
    // A pair typed in a snippet's stop keeps the snippet: Tab goes on to the next stop.
    row("$‸$", "x ff(a)\tb", "$x \\frac{(a)}{b‸}$"),
    row("$‸$", "\\left(x)", "$\\left(x\\right)‸$"),
    row("$‸$", "[0,1)", "$[0,1)‸$"),
    row("Let ‸", "$x$ be", "Let $x$ be‸"),
    row("% note ‸", "{$", "% note {$‸"),
    row("a ‸", "{⌫", "a ‸"),
    row(
        "‸",
        "\\begin{theorem}[Main]\nx",
        "\\begin{theorem}[Main]\n  x‸\n\\end{theorem}",
    ),
    row(
        "‸",
        "\\begin{enumerate}\nfirst\n\nNext",
        "\\begin{enumerate}\n  \\item first\n\\end{enumerate}\nNext‸",
    ),
];

fn real_child() {
    let mut config = config(false);
    config.completion.auto = true;
    let f = fixture_with("real", config);
    let mut typist = Real::new(&f);
    for vim in [false, true] {
        f.state.set_vim_enabled(vim);
        common::pump_for(Duration::from_millis(30));
        let label = if vim { "real vim insert" } else { "real plain" };
        run_rows(&f, &mut typist, REAL_ROWS, label);
        // A paste is not typing: Ctrl+V in plain editing (in Vim insert mode Ctrl+V inserts the
        // next key literally, so the clipboard is pasted as the context menu does).
        f.editor.view().clipboard().set_text("{$");
        common::pump_for(Duration::from_millis(20));
        typist.start(&f, "a ‸");
        if vim {
            f.editor.view().emit_paste_clipboard();
        } else {
            typist.key(
                &f,
                broadway_input::keyval('v'),
                gdk::ModifierType::CONTROL_MASK,
            );
        }
        f.wait("the paste", || f.shown() == "a {$‸");
        common::pump_for(Duration::from_millis(20));
        assert_eq!(f.shown(), "a {$‸", "{label}: a paste");
        // Enter with a word popup open (nothing selected): the line break gets its `\item`.
        typist.start(
            &f,
            "Lorem ipsum dolor.\n\\begin{itemize}\n  \\item ‸\n\\end{itemize}",
        );
        completion::refresh_project(&f.state);
        common::pump_until(TIMEOUT, || !completion::project_busy(&f.state));
        typist.text(&f, "Lor");
        f.wait("the word popup", || completion::popup_visible(&f.editor));
        typist.text(&f, "\n");
        f.wait("the new item", || {
            f.shown().contains("\\item Lor\n  \\item ‸")
        });
        typist.escape(&f);
    }
}

// ───────────── Performance ─────────────

/// Main-thread time of the helper keys (`{`, `}` typed over, `$`, Backspace on a pair, Enter in
/// a list and in plain text) on a 5k-line document: the key layer, the edit and the document's
/// observers. Budget (ARCHITECTURE §7): ≤ 2 ms p99 per keystroke, checked in release builds.
fn perf_typing_keys() {
    let f = fixture("perf", false);
    let paragraph = "Some text with $x^2 + y$ and \\emph{words} in it, \\cite{key}.\n";
    let mut doc = String::from("\\documentclass{article}\n\\begin{document}\n");
    for i in 0..1000 {
        doc.push_str(&format!("\\section{{S{i}}}\n"));
        doc.push_str(paragraph);
        doc.push_str("\\begin{itemize}\n  \\item one\n\\end{itemize}\n");
    }
    doc.push_str("\\begin{itemize}\n  \\item last‸\n\\end{itemize}\nEnd.\n\\end{document}\n");
    let mut samples = Vec::new();
    for round in 0..60 {
        f.set(&doc);
        let mut press = |key: gdk::Key| {
            let event = f.event(key, gdk::ModifierType::empty());
            let started = Instant::now();
            let _ = keys::dispatch(f.editor.view(), &event);
            samples.push(started.elapsed());
        };
        press(gdk::Key::braceleft);
        press(gdk::Key::braceright);
        press(gdk::Key::dollar);
        press(gdk::Key::BackSpace);
        press(gdk::Key::Return);
        if round == 0 {
            assert!(
                f.shown().contains("\\item last{}\n  \\item ‸"),
                "{}",
                f.shown()
            );
        }
        // Plain text far from any list: the backward scan is capped.
        let end = f.buffer().end_iter();
        f.buffer().place_cursor(&end);
        press(gdk::Key::Return);
    }
    samples.sort();
    let p99 = samples[samples.len() * 99 / 100];
    println!(
        "typing keys: {} samples, median {:?}, p99 {p99:?}",
        samples.len(),
        samples[samples.len() / 2]
    );
    if !cfg!(debug_assertions) {
        assert!(p99 <= Duration::from_millis(2), "p99 {p99:?} > 2 ms");
    }
}
