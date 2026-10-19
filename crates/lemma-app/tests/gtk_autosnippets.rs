//! Autosnippets through typed characters (ARCHITECTURE §5 "Snippets").
//!
//! - **Real input** (`typed_characters_on_real_key_events`): a private Broadway daemon and a
//!   child process (see `broadway_input`); every key takes GDK's real path through the key
//!   layer, Vim and the text view's input method. It covers the whole fire/no-fire table
//!   (below) in plain editing and in Vim insert mode, pastes, undo and redo, IM preedit,
//!   `if_undefined` bodies, an agent edit during an active snippet, and
//!   `typing.autosnippets = false`.
//! - **In process**: the same table through `keys::dispatch` and the text view's insertion, as
//!   in M1 (fast, and a check that `dispatch` agrees with real input), and edits that are not
//!   typing.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_autosnippets`.

mod broadway_input;
mod common;

use adw::prelude::*;
use broadway_input::{Browser, CONTROL};
use gtk::gdk;
use lemma_agent::EditorRequest;
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::{Editor, VimMode, completion, vim};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

const REAL_CHILD: &str = "real_input_typed_characters";

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[(REAL_CHILD, real_child)]);
    }
    common::run(&[
        (
            "typed_characters_on_real_key_events",
            typed_characters_on_real_key_events,
        ),
        (
            "fire_table_through_the_key_layer_plain",
            fire_table_in_process_plain,
        ),
        (
            "fire_table_through_the_key_layer_vim_insert",
            fire_table_in_process_vim,
        ),
        (
            "paste_undo_and_agent_edits_never_type",
            non_typing_edits_in_process,
        ),
        ("snippet_sets_reload_live", snippet_sets_reload_live),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(10);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ───────────── Fixture ─────────────

/// Lines: 2 text, 3 a comment, 5 display math, 7 an empty text line (two spaces).
const DOC: &str = "\\documentclass{article}\n\\begin{document}\nText: \n% note: \n\\[\n  \n\\]\n  \n\\end{document}\n";
const TEXT_LINE: i32 = 2;
const COMMENT_LINE: i32 = 3;
const MATH_LINE: i32 = 5;
const BLANK_LINE: i32 = 7;

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
    // No personal word lists in tests; spell checking (WP2.9b) is not under test here.
    config.spell.word_lists = Vec::new();
    config.spell.enabled = false;
    // Never a real agent.
    config.agent.node = "/nonexistent/lemma-test/node".into();
    config.agent.claude = "/nonexistent/lemma-test/claude".into();
    config
}

fn fixture(name: &str, text: &str, vim: bool) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    std::fs::write(dir.join("main.tex"), text).unwrap();
    let mut config = config(vim);
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
    // The view is the window's focus widget. (Not `has_focus`: on the shared Broadway display
    // another test process's window may be the active one.)
    common::pump_until(TIMEOUT, || editor.view().is_focus());
    let f = Fixture { state, editor, dir };
    f.settle_index();
    f
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

impl Fixture {
    fn buffer(&self) -> &sourceview5::Buffer {
        self.editor.buffer()
    }

    fn text(&self) -> String {
        self.editor.document().text()
    }

    fn line(&self, line: i32) -> String {
        self.text()
            .split('\n')
            .nth(usize::try_from(line).unwrap())
            .unwrap_or_default()
            .to_owned()
    }

    fn mode(&self) -> Option<VimMode> {
        self.editor.vim_status().map(|s| s.mode)
    }

    /// Re-indexes the project and waits for the feed.
    fn settle_index(&self) {
        completion::refresh_project(&self.state);
        common::pump_until(TIMEOUT, || !completion::project_busy(&self.state));
    }

    /// Puts `text` in the buffer with the cursor at the end of `line`.
    fn set(&self, text: &str, line: i32) {
        let buffer = self.buffer();
        buffer.set_text(text);
        let mut iter = buffer.iter_at_line(line).expect("the line exists");
        if !iter.ends_line() {
            iter.forward_to_line_end();
        }
        buffer.place_cursor(&iter);
        common::pump_for(Duration::from_millis(5));
    }

    fn wait(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + TIMEOUT;
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}; text {:?}",
                self.text()
            );
            common::pump_for(Duration::from_millis(5));
        }
    }
}

// ───────────── The fire / no-fire table ─────────────

/// One row: on `line`, type `typed`, then the line reads `expected`.
struct Row {
    line: i32,
    typed: &'static str,
    expected: &'static str,
}

const fn row(line: i32, typed: &'static str, expected: &'static str) -> Row {
    Row {
        line,
        typed,
        expected,
    }
}

const ROWS: &[Row] = &[
    // `xff` ✗, `x ff` ✓, `)ff` ✓.
    row(MATH_LINE, "xff", "  xff"),
    row(MATH_LINE, "x ff", "  x \\frac{}{}"),
    row(MATH_LINE, ")ff", "  )\\frac{}{}"),
    // `xtd` ✓ (in-word).
    row(MATH_LINE, "xtd", "  x^{}"),
    // `\sum` ✗: never right after `\`.
    row(MATH_LINE, "\\sum", "  \\sum"),
    // `\mathbb` typed letter by letter: `bb` never fires (after `h`).
    row(MATH_LINE, "\\mathbb", "  \\mathbb"),
    // `a__` ✓.
    row(MATH_LINE, "a__", "  a_{}"),
    // `mk` in a comment ✗, in math ✗ (text only), in text ✓.
    row(COMMENT_LINE, " mk", "% note:  mk"),
    row(MATH_LINE, "mk", "  mk"),
    row(TEXT_LINE, "mk", "Text: $$"),
    // `ff` in text ✗.
    row(TEXT_LINE, "ff", "Text: ff"),
    // More of the defaults: in-word arrows and relations, sets with `if_undefined`, delimiters.
    row(MATH_LINE, "a->", "  a\\to"),
    row(MATH_LINE, "x<=", "  x\\leq"),
    row(MATH_LINE, "x NN", "  x \\mathbb{N}"),
    row(MATH_LINE, "lr(", "  \\left(  \\right)"),
];

/// The manual rows: `thm` with only spaces before it expands on Tab; `x thm` does not.
const THM_EXPANDED: &str = "  \\begin{theorem}\n    \n  \\end{theorem}";

/// Something that types: real key events or the in-process emulation.
trait Typist {
    /// Presses `key` with GDK modifiers `state`.
    fn key(&mut self, f: &Fixture, key: gdk::Key, state: gdk::ModifierType);

    /// Types `text` (`\t` = Tab, `\n` = Return).
    fn text(&mut self, f: &Fixture, text: &str) {
        for c in text.chars() {
            let key = match c {
                '\t' => gdk::Key::Tab,
                '\n' => gdk::Key::Return,
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

    /// Puts `text` in the buffer and starts typing at the end of `line` (in Vim: `A`).
    fn start(&mut self, f: &Fixture, text: &str, line: i32) {
        if f.editor.vim().is_some() {
            if f.mode() != Some(VimMode::Normal) {
                self.escape(f);
            }
            f.set(text, line);
            self.key(f, gdk::Key::A, gdk::ModifierType::SHIFT_MASK);
            f.wait("insert mode", || f.mode() == Some(VimMode::Insert));
        } else {
            self.escape(f);
            f.set(text, line);
        }
    }
}

/// Real key events through the private Broadway daemon. Each key waits until the editor's key
/// layer has seen it (a probe handler counts them), so the text is checked after the key took
/// effect.
struct Real {
    browser: Browser,
    seen: Rc<std::cell::Cell<u64>>,
}

impl Real {
    fn new(f: &Fixture) -> Self {
        let seen: Rc<std::cell::Cell<u64>> = Rc::default();
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
            mods |= broadway_input::SHIFT;
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

/// What GTK does with a key, emulated: the key layer, then Vim, then the text view's insertion
/// (a Tab the layer lets through indents by two spaces, as the view does).
struct Emulated;

impl Typist for Emulated {
    fn key(&mut self, f: &Fixture, keyval: gdk::Key, state: gdk::ModifierType) {
        let display = WidgetExt::display(&f.state.window);
        let keycode = display
            .map_keyval(keyval)
            .and_then(|keys| keys.into_iter().min_by_key(gdk::KeymapKey::level))
            .map_or(0, |k| k.keycode());
        let event = KeyEvent {
            keyval,
            keycode,
            state,
            vim: f.editor.vim().is_some(),
        };
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
                let text = match keyval {
                    gdk::Key::Return => Some("\n".to_owned()),
                    gdk::Key::Tab => Some("  ".to_owned()),
                    k => k.to_unicode().filter(|c| !c.is_control()).map(String::from),
                };
                if let Some(text) = text {
                    f.buffer().insert_interactive_at_cursor(&text, true);
                }
            }
        }
        common::pump_for(Duration::from_millis(2));
    }
}

/// Runs the table with `typist`; `label` names the mode in failures.
fn fire_table(f: &Fixture, typist: &mut dyn Typist, label: &str) {
    for row in ROWS {
        typist.start(f, DOC, row.line);
        typist.text(f, row.typed);
        assert_eq!(
            f.line(row.line),
            row.expected,
            "{label}: typing {:?}",
            row.typed
        );
    }
    // The cursor waits in the first stop.
    typist.start(f, DOC, MATH_LINE);
    typist.text(f, "x ffa");
    assert_eq!(f.line(MATH_LINE), "  x \\frac{a}{}", "{label}: first stop");
    // `mk` leaves the cursor between the dollars.
    typist.start(f, DOC, TEXT_LINE);
    typist.text(f, "mkx");
    assert_eq!(f.line(TEXT_LINE), "Text: $x$", "{label}: mk");
    // `thm` with only spaces before it: Tab expands the manual snippet.
    typist.start(f, DOC, BLANK_LINE);
    typist.text(f, "thm\t");
    assert!(
        f.text().contains(THM_EXPANDED),
        "{label}: thm + Tab at the line start: {:?}",
        f.text()
    );
    // `x thm`: no line-begin match, Tab indents.
    typist.start(f, DOC, TEXT_LINE);
    typist.text(f, "x thm\t");
    assert!(
        f.line(TEXT_LINE).starts_with("Text: x thm") && !f.text().contains("\\begin{theorem}"),
        "{label}: x thm + Tab: {:?}",
        f.text()
    );
    typist.escape(f);
}

// ───────────── Real input ─────────────

fn typed_characters_on_real_key_events() {
    broadway_input::run_child(REAL_CHILD, 1);
}

/// The child: everything on real key events, plain and in Vim insert mode.
fn real_child() {
    let f = fixture("real", DOC, false);
    let mut typist = Real::new(&f);
    for vim in [false, true] {
        f.state.set_vim_enabled(vim);
        common::pump_for(Duration::from_millis(30));
        let label = if vim { "vim insert" } else { "plain" };
        fire_table(&f, &mut typist, label);
        undo_gives_back_the_trigger(&f, &mut typist, vim);
        paste_never_expands(&f, &mut typist, vim);
        agent_edit_during_a_snippet(&f, &mut typist);
        if_undefined_bodies(&f, &mut typist);
        typist.escape(&f);
    }
    f.state.set_vim_enabled(false);
    preedit_never_expands(&f, &mut typist);
    switched_off(&f, &mut typist);
}

/// One undo after an expansion gives the literal trigger back (Ctrl+Z, or `u` after `<Esc>`);
/// redo brings the expansion back.
fn undo_gives_back_the_trigger(f: &Fixture, typist: &mut dyn Typist, vim: bool) {
    typist.start(f, DOC, MATH_LINE);
    typist.text(f, "x ff");
    assert_eq!(f.line(MATH_LINE), "  x \\frac{}{}");
    if vim {
        typist.escape(f);
        typist.text(f, "u");
    } else {
        typist.key(
            f,
            broadway_input::keyval('z'),
            gdk::ModifierType::CONTROL_MASK,
        );
    }
    f.wait("the undo", || f.line(MATH_LINE) != "  x \\frac{}{}");
    assert_eq!(f.line(MATH_LINE), "  x ff", "undo gives back the trigger");
    if vim {
        typist.key(
            f,
            broadway_input::keyval('r'),
            gdk::ModifierType::CONTROL_MASK,
        );
    } else {
        typist.key(
            f,
            broadway_input::keyval('z'),
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
        );
    }
    f.wait("the redo", || f.line(MATH_LINE) == "  x \\frac{}{}");
}

/// A paste of `ff` in math is not typing.
fn paste_never_expands(f: &Fixture, typist: &mut dyn Typist, vim: bool) {
    if vim {
        // Yank `ff` and put it after `x `.
        let doc = DOC.replace("\\[\n  \n", "\\[\n  ff x \n");
        typist.start(f, &doc, MATH_LINE);
        typist.escape(f);
        let start = f.buffer().iter_at_line_offset(MATH_LINE, 2).unwrap();
        f.buffer().place_cursor(&start);
        typist.text(f, "y2l$p");
        f.wait("the put", || f.line(MATH_LINE) == "  ff x ff");
    } else {
        f.editor.view().clipboard().set_text("ff");
        common::pump_for(Duration::from_millis(20));
        typist.start(f, DOC, MATH_LINE);
        typist.text(f, "x ");
        typist.key(
            f,
            broadway_input::keyval('v'),
            gdk::ModifierType::CONTROL_MASK,
        );
        f.wait("the paste", || f.line(MATH_LINE) == "  x ff");
    }
    common::pump_for(Duration::from_millis(30));
    assert!(
        !f.text().contains("\\frac"),
        "a paste expanded: {:?}",
        f.text()
    );
}

/// An agent edit while a snippet is active ends the session first; nothing breaks.
fn agent_edit_during_a_snippet(f: &Fixture, typist: &mut dyn Typist) {
    typist.start(f, DOC, MATH_LINE);
    typist.text(f, "x ff");
    assert!(completion::snippet_active(&f.state, &f.editor));
    let reply = block_on(lemma_app::agent_edits::apply(
        &f.state.workspace,
        7,
        EditorRequest::Edit {
            path: f.dir.join("main.tex"),
            old_string: "Text: ".into(),
            new_string: "Changed text: ".into(),
            replace_all: false,
        },
    ));
    assert!(!reply.is_error, "{reply:?}");
    common::pump_for(Duration::from_millis(30));
    assert!(
        !completion::snippet_active(&f.state, &f.editor),
        "the agent edit ended the session"
    );
    assert_eq!(f.line(TEXT_LINE), "Changed text: ");
    // Typing goes on where the cursor is; Tab no longer jumps between stops but steps out.
    typist.text(f, "a\tb");
    assert_eq!(f.line(MATH_LINE), "  x \\frac{a}b{}");
}

/// `hom` uses the project's `\Hom` when it is defined, else `\operatorname{Hom}`.
fn if_undefined_bodies(f: &Fixture, typist: &mut dyn Typist) {
    typist.start(f, DOC, MATH_LINE);
    f.settle_index();
    assert!(!completion::macro_defined(&f.state, "Hom"));
    typist.text(f, "x hom");
    assert_eq!(f.line(MATH_LINE), "  x \\operatorname{Hom}");
    let defined = DOC.replace(
        "\\begin{document}",
        "\\DeclareMathOperator{\\Hom}{Hom}\n\\begin{document}",
    );
    typist.start(f, &defined, MATH_LINE + 1);
    f.settle_index();
    assert!(completion::macro_defined(&f.state, "Hom"));
    typist.text(f, "x hom");
    assert_eq!(f.line(MATH_LINE + 1), "  x \\Hom");
}

/// A key that completes an IM compose sequence (dead key + `f`) is not a typed `f`.
fn preedit_never_expands(f: &Fixture, typist: &mut dyn Typist) {
    typist.start(f, DOC, MATH_LINE);
    typist.text(f, "x f");
    typist.key(f, gdk::Key::dead_acute, gdk::ModifierType::empty());
    typist.text(f, "f");
    common::pump_for(Duration::from_millis(30));
    assert!(
        !f.line(MATH_LINE).contains("\\frac"),
        "a compose sequence expanded: {:?}",
        f.line(MATH_LINE)
    );
    typist.escape(f);
}

/// `typing.autosnippets = false` switches them off (read at use time: a reload applies at once).
fn switched_off(f: &Fixture, typist: &mut dyn Typist) {
    let mut config = (*f.state.config.get()).clone();
    config.typing.autosnippets = false;
    f.state.set_config(Rc::new(config.clone()));
    typist.start(f, DOC, MATH_LINE);
    typist.text(f, "x ff");
    assert_eq!(f.line(MATH_LINE), "  x ff");
    config.typing.autosnippets = true;
    f.state.set_config(Rc::new(config));
    typist.text(f, " ff");
    assert_eq!(f.line(MATH_LINE), "  x ff \\frac{}{}");
    typist.escape(f);
}

// ───────────── In process ─────────────

fn fire_table_in_process_plain() {
    let f = fixture("table-plain", DOC, false);
    fire_table(&f, &mut Emulated, "plain (dispatch)");
}

fn fire_table_in_process_vim() {
    let f = fixture("table-vim", DOC, true);
    fire_table(&f, &mut Emulated, "vim insert (dispatch)");
}

/// Edits that are not key presses of a character never expand: text inserted in one go (a
/// paste, a drop), single characters inserted without a key press, undo/redo, agent edits.
fn non_typing_edits_in_process() {
    let f = fixture("non-typing", DOC, false);
    let mut typist = Emulated;
    typist.start(&f, DOC, MATH_LINE);
    typist.text(&f, "x ");
    f.buffer().insert_interactive_at_cursor("ff", true);
    common::pump_for(Duration::from_millis(10));
    assert_eq!(f.line(MATH_LINE), "  x ff");
    f.buffer().insert_interactive_at_cursor(" f", true);
    f.buffer().insert_interactive_at_cursor("f", true);
    common::pump_for(Duration::from_millis(10));
    assert_eq!(f.line(MATH_LINE), "  x ff ff");
    f.buffer().undo();
    f.buffer().redo();
    common::pump_for(Duration::from_millis(10));
    assert!(!f.text().contains("\\frac"), "{:?}", f.text());
    let reply = block_on(lemma_app::agent_edits::apply(
        &f.state.workspace,
        3,
        EditorRequest::Edit {
            path: f.dir.join("main.tex"),
            old_string: "  x ff ff\n".into(),
            new_string: "  x ff ff ff\n".into(),
            replace_all: false,
        },
    ));
    assert!(!reply.is_error, "{reply:?}");
    common::pump_for(Duration::from_millis(10));
    assert!(!f.text().contains("\\frac"), "{:?}", f.text());
    // Typing still works afterwards.
    typist.start(&f, DOC, MATH_LINE);
    typist.text(&f, "x ff");
    assert_eq!(f.line(MATH_LINE), "  x \\frac{}{}");
}

/// A reloaded `snippets.toml` applies to the next typed character.
fn snippet_sets_reload_live() {
    let f = fixture("reload", DOC, false);
    let user = lemma_snippets::SnippetSet::from_toml(
        "[[snippet]]\ntrigger = 'ff'\nbody = '\\dfrac{$1}{$2}'\nauto = true\ncontext = 'math'\n",
    )
    .unwrap();
    f.state
        .set_snippets(Rc::new(lemma_snippets::SnippetSet::merged(
            lemma_snippets::SnippetSet::defaults(),
            user,
        )));
    let mut typist = Emulated;
    typist.start(&f, DOC, MATH_LINE);
    typist.text(&f, "x ff");
    assert_eq!(f.line(MATH_LINE), "  x \\dfrac{}{}");
}
