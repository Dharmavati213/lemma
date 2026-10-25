//! The key layer and Lemma's Vim bindings (WP2.10, ARCHITECTURE §4.3, risk §9.3).
//!
//! Two kinds of tests:
//!
//! - **In-process**, as in M1: a key press is emulated the way GTK delivers it — the key layer
//!   first ([`keys::dispatch`]), then Vim (`gtk_im_context_filter_key`, which builds the same
//!   events a keyboard produces), then, for a printable character Vim lets through, the text
//!   view's insertion.
//! - **Real input** (`layer_runs_before_vim_on_real_key_events`): GTK cannot inject events, so the
//!   test starts a private `gtk4-broadwayd` (its own display and loopback port, so no other test
//!   window can receive the keys), runs this binary again as a child on that display, and the
//!   child sends key events through the daemon's browser protocol (a WebSocket on 127.0.0.1).
//!   They take GDK's real path: application accelerators, then the capture phase from the window
//!   down — the layer's controller on the scrolled window, Vim's controller on the view — then
//!   the text view. This is the evidence for the key ordering the app relies on.
//!
//! No test can start a real agent: the fixture's configuration points at no adapter, the window
//! has no `Paths`, and the agent scenario does not exist.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_keys`.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_agent::{AgentEvent, EditorRequest, ToolReply};
use lemma_app::agent::{AgentState, SentPrompt};
use lemma_app::agent_edits;
use lemma_app::diagnostics_panel::Problem;
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::{Editor, TexView, VimMode, vim};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::{DiagSource, ProjectRoot, Severity};
use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Set (to the daemon's port) in the child process of the real-input test.
const CHILD_PORT_ENV: &str = "LEMMA_TEST_BROADWAY_PORT";

fn main() -> ExitCode {
    if std::env::var_os(CHILD_PORT_ENV).is_some() {
        return common::run(&[("real_input_child", real_input::child)]);
    }
    common::run(&[
        (
            "layer_runs_before_vim_on_real_key_events",
            real_input::parent,
        ),
        (
            "bracket_h_in_normal_mode_moves_between_hunks",
            bracket_h_moves_between_hunks,
        ),
        (
            "bracket_h_in_insert_mode_types_the_characters",
            bracket_h_in_insert_mode_types,
        ),
        (
            "bracket_e_goes_to_the_next_and_previous_problem",
            bracket_e_goes_to_problems,
        ),
        (
            "tab_in_insert_mode_reaches_the_snippet_handler_before_vim",
            tab_reaches_snippet_handler_first,
        ),
        ("chords_work_in_every_mode", chords_work_in_every_mode),
        ("ga_and_gr_decide_the_hunk_at_the_cursor", ga_gr_decide),
        ("counts_repeat_the_review_bindings", counts_repeat_bindings),
        (
            "held_keys_reach_vim_when_no_binding_matches",
            held_keys_reach_vim,
        ),
        (
            "pending_vim_commands_take_the_keys",
            pending_vim_commands_take_the_keys,
        ),
        (
            "space_a_and_remapped_bindings_run_their_actions",
            space_a_and_remapping,
        ),
        (
            "bindings_follow_the_vim_toggle_and_tab_close",
            bindings_follow_vim_toggle,
        ),
        ("ex_commands_with_counts", ex_commands_with_counts),
        (
            "ex_agent_commands_reach_the_agent",
            ex_agent_commands_reach_the_agent,
        ),
        (
            "insert_mode_ctrl_w_deletes_the_word_before_the_cursor",
            ctrl_w_deletes_the_word_before_the_cursor,
        ),
    ])
}

/// Vim's insert-mode Ctrl-W, which GtkSourceView 5.20 lacks (Lemma gives Ctrl+W to Vim while it
/// is on, see the real-input test): the blanks before the cursor and the word before them, a
/// run of other characters as a word of its own, the line break at a line start.
fn ctrl_w_deletes_the_word_before_the_cursor() {
    let f = fixture("ctrl-w", &main_files());
    let buffer = f.editor.buffer();
    for (text, at, expected) in [
        ("line 1 foo bar", 14, "line 1 foo "),
        ("line 1 foo   ", 13, "line 1 "),
        ("x = \\frac", 10, "x = \\"),
        ("x = \\", 5, "x = "),
        ("a_b2 c", 4, " c"),
        ("ab\ncd", 3, "abcd"),
        ("", 0, ""),
    ] {
        buffer.set_text(text);
        buffer.place_cursor(&buffer.iter_at_offset(at));
        keys::delete_word_before_cursor(f.view());
        assert_eq!(f.text(), expected, "{text:?} at {at}");
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ----- fixture -----

struct Fixture {
    state: Rc<WindowState>,
    editor: Rc<Editor>,
    dir: PathBuf,
    /// Activations of the stand-in actions (`agent-focus`, `agent-inline-edit`, `test-*`).
    actions: Rc<RefCell<Vec<String>>>,
}

/// Twelve numbered lines; [`Fixture::with_hunks`] has the agent change lines 2, 6 and 10.
const MAIN: &str = "line 0\nline 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\n\
                    line 9\nline 10\nline 11\n";

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
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
    config.build.on_agent_turn_end = false;
    config.editor.vim = true;
    // Never a real agent: no adapter can be found here (and the window has no `Paths`).
    config.agent.node = "/nonexistent/lemma-test/node".into();
    config.agent.claude = "/nonexistent/lemma-test/claude".into();
    config.agent.adapter_dir = dir.join(".no-adapter");
    let app = common::application();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, true);
    // What the application does at start-up (F10 and the other default accelerators).
    lemma_app::app::set_accels(&app, &state.config.keys);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    if let Some(agent) = state.agent_controller() {
        agent.set_scenario(Some(dir.join("no-such-scenario.json")));
    }
    let actions: Rc<RefCell<Vec<String>>> = Rc::default();
    // Stand-ins for WP2.5's actions, so the tests see exactly when they run.
    for name in [
        "agent-focus",
        "agent-inline-edit",
        "test-one",
        "test-two",
        "test-three",
    ] {
        let action = gio::SimpleAction::new(name, None);
        let log = actions.clone();
        action.connect_activate(move |_, _| log.borrow_mut().push(name.to_owned()));
        state.window.add_action(&action);
    }
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    let editor = block_on(state.open_file(&dir.join(files[0].0))).expect("open");
    // The view is its window's focus widget. (Not `has_focus`, which also needs the window to be
    // the active one: on the shared Broadway display other test processes' windows compete.)
    common::pump_until(TIMEOUT, || editor.view().is_focus());
    Fixture {
        state,
        editor,
        dir,
        actions,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// The key value and modifiers of one key name in Vim notation (`]`, `<Space>`, `<C-k>`).
fn key_of(name: &str) -> (gdk::Key, gdk::ModifierType) {
    let Some(inner) = name.strip_prefix('<').and_then(|n| n.strip_suffix('>')) else {
        return (
            keyval(name.chars().next().unwrap()),
            gdk::ModifierType::empty(),
        );
    };
    let mut state = gdk::ModifierType::empty();
    let mut rest = inner;
    loop {
        if let Some(r) = rest.strip_prefix("C-") {
            state |= gdk::ModifierType::CONTROL_MASK;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("M-") {
            state |= gdk::ModifierType::ALT_MASK;
            rest = r;
        } else {
            break;
        }
    }
    let key = match rest {
        "Space" => gdk::Key::space,
        "Esc" => gdk::Key::Escape,
        "CR" => gdk::Key::Return,
        "Tab" => gdk::Key::Tab,
        "BS" => gdk::Key::BackSpace,
        "lt" => gdk::Key::less,
        "F10" => gdk::Key::F10,
        one if one.chars().count() == 1 => keyval(one.chars().next().unwrap()),
        other => panic!("no test key for <{other}>"),
    };
    (key, state)
}

fn keyval(c: char) -> gdk::Key {
    // `gdk::Key` has no safe constructor from a keyval; it is a `u32` GValue.
    gdk::unicode_to_keyval(u32::from(c))
        .to_value()
        .get::<gdk::Key>()
        .expect("a keyval")
}

impl Fixture {
    fn view(&self) -> &TexView {
        self.editor.view()
    }

    /// Presses the keys of `notation` (Vim notation, see `keys::parse_keys`) one by one, as GTK
    /// would deliver them (see the module docs), running the main loop briefly after each.
    fn keys(&self, notation: &str) {
        if notation.is_empty() {
            return;
        }
        for name in keys::parse_keys(notation).expect("key notation") {
            self.press(&name);
        }
    }

    /// One key press: the key layer, then Vim, then the text view.
    fn press(&self, name: &str) {
        let (keyval, state) = key_of(name);
        let display = WidgetExt::display(&self.state.window);
        let key = display
            .map_keyval(keyval)
            .and_then(|keys| keys.into_iter().min_by_key(gdk::KeymapKey::level))
            .unwrap_or_else(|| panic!("no keycode for {name}"));
        let state = if key.level() > 0 {
            state | gdk::ModifierType::SHIFT_MASK
        } else {
            state
        };
        let event = KeyEvent {
            keyval,
            keycode: key.keycode(),
            state,
            vim: self.editor.vim().is_some(),
        };
        if keys::dispatch(self.view(), &event) == glib::Propagation::Proceed
            && !self.editor.vim_blocks_key(keyval, state)
        {
            let consumed = self
                .editor
                .vim()
                .is_some_and(|im| vim::feed_key(&im, self.view(), keyval, state));
            let modified =
                state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
            if !consumed && !modified {
                // What the text view does with a key nobody consumed.
                let text = match keyval {
                    gdk::Key::Return => Some('\n'),
                    k => k.to_unicode().filter(|c| !c.is_control()),
                };
                if let Some(c) = text {
                    self.editor
                        .buffer()
                        .insert_interactive_at_cursor(&c.to_string(), true);
                }
            }
        }
        common::pump_for(Duration::from_millis(3));
    }

    /// `keys` straight to Vim, as if there were no key layer (for comparisons).
    fn vim_alone(&self, notation: &str) {
        let im = self.editor.vim().expect("Vim is on");
        for name in keys::parse_keys(notation).expect("key notation") {
            let (keyval, state) = key_of(&name);
            if !vim::feed_key(&im, self.view(), keyval, state)
                && let Some(c) = keyval.to_unicode().filter(|c| !c.is_control())
            {
                self.editor
                    .buffer()
                    .insert_interactive_at_cursor(&c.to_string(), true);
            }
            common::pump_for(Duration::from_millis(3));
        }
    }

    /// Back to normal mode with `text`.
    fn reset(&self, text: &str) {
        self.keys("<Esc><Esc>");
        if self.text() != text {
            self.editor.buffer().set_text(text);
        }
        assert_eq!(self.mode(), VimMode::Normal);
    }

    fn text(&self) -> String {
        self.editor.document().text()
    }

    fn mode(&self) -> VimMode {
        self.editor.vim_status().expect("Vim is on").mode
    }

    fn vim_command(&self) -> String {
        self.editor.vim_status().expect("Vim is on").command
    }

    /// 0-based (line, column) of the cursor.
    fn cursor(&self) -> (u32, u32) {
        let (line, col) = self.editor.cursor_position();
        (line - 1, col - 1)
    }

    fn set_cursor(&self, line: i32, col: i32) {
        let buffer = self.editor.buffer();
        let iter = buffer.iter_at_line_offset(line, col).expect("a position");
        buffer.place_cursor(&iter);
    }

    fn agent(&self, req: EditorRequest) -> ToolReply {
        block_on(agent_edits::apply(&self.state.workspace, 1, req))
    }

    /// An agent edit of `file`.
    fn agent_edit(&self, file: &str, old: &str, new: &str) {
        let reply = self.agent(EditorRequest::Edit {
            path: file.into(),
            old_string: old.into(),
            new_string: new.into(),
            replace_all: false,
        });
        assert!(!reply.is_error, "agent edit failed: {reply:?}");
        common::pump_for(Duration::from_millis(20));
    }

    fn doc(&self, file: &str) -> lemma_app::document::DocRef {
        let path = lemma_app::workspace::normalize(&self.dir.join(file));
        self.state
            .workspace
            .get(&path)
            .unwrap_or_else(|| panic!("{file} is not in the workspace"))
    }

    fn hunk_starts(&self, file: &str) -> Vec<u32> {
        self.doc(file)
            .review
            .borrow()
            .as_ref()
            .map(|r| r.hunks().iter().map(|h| h.cur_lines.start).collect())
            .unwrap_or_default()
    }

    /// The agent changes lines 2, 6 and 10 of `main.tex`.
    fn with_hunks(&self) {
        self.agent_edit("main.tex", "line 2\n", "LINE TWO\n");
        self.agent_edit("main.tex", "line 6\n", "LINE SIX\n");
        self.agent_edit("main.tex", "line 10\n", "LINE TEN\n");
        common::pump_until(TIMEOUT, || self.hunk_starts("main.tex") == [2, 6, 10]);
    }

    /// The agent changes line 1 of a new closed document `z.tex` (after `main.tex` in review
    /// order).
    fn with_second_file(&self) {
        std::fs::write(self.dir.join("z.tex"), "zero\none\n").unwrap();
        self.agent_edit("z.tex", "one\n", "ONE\n");
        common::pump_until(TIMEOUT, || self.hunk_starts("z.tex") == [1]);
    }

    /// Runs the main loop until `done`; on a timeout, fails with the editor's state.
    fn wait(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: cursor {:?}, Vim {:?}, active {:?}, hunks {:?}, \
                 text {:?}",
                self.cursor(),
                self.editor.vim_status(),
                self.state
                    .active_editor()
                    .map(|e| e.document().path().to_owned()),
                self.hunk_starts("main.tex"),
                self.text()
            );
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_cursor_line(&self, line: u32) {
        self.wait(&format!("the cursor on line {line}"), || {
            self.cursor().0 == line
        });
    }

    fn wait_active(&self, file: &str) {
        self.wait(&format!("{file} in the active tab"), || {
            self.state
                .active_editor()
                .is_some_and(|e| e.document().path().ends_with(file))
        });
    }

    /// Back to `main.tex`'s tab with the focus in its view.
    fn back_to_main(&self) {
        self.state.select_editor(&self.editor);
        self.wait("the focus in main.tex", || self.view().is_focus());
    }

    fn actions(&self) -> Vec<String> {
        self.actions.borrow().clone()
    }
}

fn main_files() -> Vec<(&'static str, &'static str)> {
    vec![("main.tex", MAIN)]
}

fn edited_main() -> String {
    MAIN.replace("line 2\n", "LINE TWO\n")
        .replace("line 6\n", "LINE SIX\n")
        .replace("line 10\n", "LINE TEN\n")
}

// ----- in-process tests -----

fn bracket_h_moves_between_hunks() {
    let f = fixture("bracket-h", &main_files());
    f.with_hunks();
    f.set_cursor(0, 0);
    assert_eq!(f.mode(), VimMode::Normal);
    f.keys("]");
    assert_eq!(
        f.state.status.vim_command.text(),
        "]",
        "the held key shows in the status bar"
    );
    assert_eq!(f.vim_command(), "", "Vim never saw the ]");
    f.keys("h");
    f.wait_cursor_line(2);
    assert_eq!(f.state.status.vim_command.text(), "");
    for (keys, line) in [("]h", 6), ("]h", 10), ("]h", 2), ("[h", 10), ("[h", 6)] {
        f.keys(keys);
        f.wait_cursor_line(line);
    }
    assert_eq!(f.mode(), VimMode::Normal);
    assert_eq!(f.vim_command(), "");
    assert_eq!(f.text(), edited_main(), "navigation changes no text");

    // Across files: the next file with hunks is opened.
    f.with_second_file();
    f.set_cursor(10, 0);
    f.keys("]h");
    f.wait_active("z.tex");
    let z = f.state.active_editor().unwrap();
    common::pump_until(TIMEOUT, || z.cursor_position().0 == 2);
}

fn bracket_h_in_insert_mode_types() {
    let f = fixture("insert-h", &main_files());
    f.with_hunks();
    f.set_cursor(0, 0);
    f.keys("i]h<Esc>");
    assert_eq!(f.mode(), VimMode::Normal);
    assert!(f.text().starts_with("]hline 0\n"), "{:?}", f.text());
    assert_eq!(f.cursor().0, 0, "no hunk navigation");
    // With Vim off, `]h` is plain typing too.
    f.state.set_vim_enabled(false);
    let event = keys::event_for(
        &f.editor,
        gdk::Key::bracketright,
        gdk::ModifierType::empty(),
    );
    assert_eq!(keys::dispatch(f.view(), &event), glib::Propagation::Proceed);
}

/// `]e` / `[e` (`[vim.normal]` defaults: next and previous build problem) go from the cursor to
/// the problems in the Problems page's order, around the end. Vim has no `]e` or `[e`
/// (GtkSourceView's Vim drops the bracket and moves to the end of the word, as `e` alone does),
/// so the bindings take no Vim command.
fn bracket_e_goes_to_problems() {
    let f = fixture("bracket-e", &main_files());
    for keys in ["]e", "[e"] {
        f.set_cursor(4, 2);
        f.vim_alone(keys);
        let alone = (f.text(), f.cursor(), f.mode(), f.vim_command());
        f.set_cursor(4, 2);
        f.vim_alone("e");
        assert_eq!(
            (f.text(), f.cursor(), f.mode(), f.vim_command()),
            alone,
            "{keys}"
        );
    }
    let path = lemma_app::workspace::normalize(&f.dir.join("main.tex"));
    let problem = |line| Problem {
        path: path.clone(),
        line: Some(line),
        severity: Severity::Error,
        message: "Undefined control sequence.".into(),
        context: None,
        source: DiagSource::Tex,
    };
    let panel = f.state.build.get().unwrap().diagnostics();
    // 1-based lines: the cursor goes to the 0-based lines 3 and 9.
    panel.set(vec![problem(4), problem(10)], &path);
    f.set_cursor(0, 0);
    for (keys, line) in [("]e", 3), ("]e", 9), ("]e", 3), ("[e", 9), ("[e", 3)] {
        f.keys(keys);
        f.wait_cursor_line(line);
    }
    assert_eq!(f.mode(), VimMode::Normal);
    assert_eq!(f.vim_command(), "");
    assert_eq!(f.text(), MAIN, "navigation changes no text");
}

fn tab_reaches_snippet_handler_first() {
    let f = fixture("tab", &main_files());
    type Seen = Rc<RefCell<Vec<(gdk::Key, VimMode, String)>>>;
    let seen: Seen = Rc::default();
    let log = seen.clone();
    let ed = Rc::downgrade(&f.editor);
    // At the snippet driver's priority (just above, so a snippet handler that stops Tab itself
    // cannot hide it from the probe).
    let id = keys::register(
        f.view(),
        keys::PRIORITY_SNIPPETS + 1,
        move |event: &KeyEvent, mode: VimMode| {
            let text = ed
                .upgrade()
                .map(|e| e.document().text())
                .unwrap_or_default();
            log.borrow_mut().push((event.keyval, mode, text));
            if event.keyval == gdk::Key::Tab && mode == VimMode::Insert {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        },
    )
    .unwrap();
    f.set_cursor(0, 0);
    f.keys("i");
    let before = f.text();
    f.keys("<Tab>");
    assert_eq!(
        f.text(),
        before,
        "the stopped Tab reached neither Vim nor the view"
    );
    assert_eq!(f.mode(), VimMode::Insert);
    let last = seen.borrow().last().cloned().unwrap();
    assert_eq!(last, (gdk::Key::Tab, VimMode::Insert, before));
    // The snippet priority runs before the Vim bindings: a `]` that the bindings hold went
    // through the probe first.
    f.keys("<Esc>");
    seen.borrow_mut().clear();
    f.keys("]");
    assert_eq!(seen.borrow()[0].0, gdk::Key::bracketright);
    assert_eq!(f.state.status.vim_command.text(), "]");
    f.keys("<Esc>");
    assert_eq!(f.state.status.vim_command.text(), "");
    keys::unregister(f.view(), id);
}

fn chords_work_in_every_mode() {
    let f = fixture("chords", &main_files());
    f.with_hunks();
    let app = &f.state.app;
    for (action, accel) in [
        ("win.review-next", "<Alt>bracketright"),
        ("win.review-prev", "<Alt>bracketleft"),
        ("win.review-accept", "<Alt>y"),
        ("win.review-reject", "<Alt>n"),
        ("win.toggle-agent", "F10"),
        ("win.agent-inline-edit", "<Control>k"),
    ] {
        assert!(
            app.accels_for_action(action).iter().any(|a| a == accel),
            "{action} has {accel}: {:?}",
            app.accels_for_action(action)
        );
    }
    // Accelerators run before the key layer and Vim (checked on real key events in
    // `layer_runs_before_vim_on_real_key_events`). Here: their actions work in every mode and
    // leave the mode and the text alone.
    let mut line = 0;
    for (enter, mode) in [
        ("", VimMode::Normal),
        ("i", VimMode::Insert),
        ("v", VimMode::Visual),
        ("V", VimMode::VisualLine),
    ] {
        f.keys("<Esc>");
        f.set_cursor(line, 0);
        f.keys(enter);
        assert_eq!(f.mode(), mode);
        let before = f.text();
        WidgetExt::activate_action(&f.state.window, "win.review-next", None).unwrap();
        let next = [2, 6, 10].into_iter().find(|s| *s > line).unwrap_or(2);
        f.wait_cursor_line(u32::try_from(next).unwrap());
        line = next;
        WidgetExt::activate_action(&f.state.window, "win.agent-inline-edit", None).unwrap();
        assert_eq!(f.mode(), mode, "{mode:?} is kept");
        assert_eq!(f.text(), before);
    }
    f.keys("<Esc>");
    let inline = f
        .actions()
        .iter()
        .filter(|a| *a == "agent-inline-edit")
        .count();
    assert_eq!(inline, 4, "{:?}", f.actions());
}

fn ga_gr_decide() {
    let f = fixture("ga-gr", &main_files());
    f.with_hunks();
    f.set_cursor(2, 0);
    f.keys("ga");
    common::pump_until(TIMEOUT, || f.hunk_starts("main.tex") == [6, 10]);
    assert!(
        f.text().contains("LINE TWO\n"),
        "accept keeps the agent's text"
    );
    f.wait_cursor_line(6);
    f.keys("gr");
    common::pump_until(TIMEOUT, || f.hunk_starts("main.tex") == [10]);
    assert!(
        f.text().contains("line 6\n"),
        "reject restores the baseline"
    );
    f.wait_cursor_line(10);
    assert_eq!(f.mode(), VimMode::Normal);
    assert_eq!(f.vim_command(), "");
    // Alt+U reverts the last decision, as after the chords.
    WidgetExt::activate_action(&f.state.window, "win.review-undo", None).unwrap();
    common::pump_until(TIMEOUT, || f.hunk_starts("main.tex") == [6, 10]);
    // No hunk at the cursor: nothing is decided.
    f.set_cursor(0, 0);
    f.keys("ga");
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.hunk_starts("main.tex"), [6, 10]);
}

fn counts_repeat_bindings() {
    let f = fixture("counts", &main_files());
    f.with_hunks();
    f.set_cursor(0, 0);
    f.keys("2");
    assert_eq!(f.vim_command(), "2");
    f.keys("]");
    assert_eq!(f.state.status.vim_command.text(), "2]");
    f.keys("h");
    f.wait_cursor_line(6);
    assert_eq!(f.vim_command(), "", "the count is cleared in Vim");
    f.keys("x");
    assert_eq!(
        f.text().lines().nth(6),
        Some("INE SIX"),
        "x deletes one character, not two"
    );
    f.keys("u");
    assert_eq!(f.text(), edited_main());
    f.keys("3[h");
    f.wait_cursor_line(6);
    f.keys("4]h");
    f.wait_cursor_line(10);

    // `2ga` decides two hunks as two Alt+Y would: the one at the cursor, then the next one
    // (wrapping within the file), and moves to the one after them.
    f.set_cursor(6, 0);
    f.keys("2ga");
    common::pump_until(TIMEOUT, || f.hunk_starts("main.tex") == [2]);
    f.wait_cursor_line(2);
    assert!(f.text().contains("LINE SIX\n") && f.text().contains("LINE TEN\n"));
    f.keys("9gr");
    common::pump_until(TIMEOUT, || f.hunk_starts("main.tex").is_empty());
    assert!(f.text().contains("line 2\n"), "{}", f.text());
    assert_eq!(f.vim_command(), "");
}

fn held_keys_reach_vim() {
    let f = fixture("held", &main_files());
    // Sequences that start like a binding but are not one end exactly as they do in Vim alone:
    // the held keys are given to Vim in order, with the count Vim had.
    for (line, col, keys) in [
        (5, 3, "gg"),
        (5, 3, "gj"),
        (5, 3, "2gjx"),
        (1, 0, "<Space>x"),
        (3, 0, "2<Space>x"),
        (3, 0, "<Space><Space>x"),
        (4, 2, "]]x"),
        (4, 2, "[x"),
        (4, 2, "g<Space>x"),
        (4, 2, "gex"),
        (0, 0, "g]"),
    ] {
        let start = f.text();
        let alone = {
            f.set_cursor(line, col);
            f.vim_alone(keys);
            let result = (f.text(), f.cursor(), f.mode());
            f.reset(&start);
            result
        };
        f.set_cursor(line, col);
        f.keys(keys);
        assert_eq!(
            (f.text(), f.cursor(), f.mode()),
            alone,
            "{keys:?} through the layer"
        );
        assert_eq!(
            f.state.status.vim_command.text(),
            f.vim_command(),
            "{keys:?}: nothing is held"
        );
        f.reset(&start);
    }
    // `<Space>` alone moves right after the timeout.
    f.set_cursor(1, 0);
    f.keys("<Space>");
    let held_at = Instant::now();
    assert_eq!(f.cursor(), (1, 0), "held");
    assert_eq!(f.state.status.vim_command.text(), "<Space>");
    f.wait("the timeout", || f.cursor() == (1, 1));
    assert!(held_at.elapsed() >= keys::SEQUENCE_TIMEOUT - Duration::from_millis(100));
    assert_eq!(f.state.status.vim_command.text(), "");
    // Modifier keys do not end a held sequence.
    f.set_cursor(4, 0);
    f.keys("g");
    let shift = KeyEvent {
        keyval: gdk::Key::Shift_L,
        keycode: 0,
        state: gdk::ModifierType::empty(),
        vim: true,
    };
    assert_eq!(keys::dispatch(f.view(), &shift), glib::Propagation::Proceed);
    assert_eq!(f.state.status.vim_command.text(), "g");
    f.keys("g");
    assert_eq!(f.cursor(), (0, 0));
}

fn pending_vim_commands_take_the_keys() {
    let f = fixture(
        "pending",
        &[("main.tex", "a]b]c\nline 1\nline 2\nline 3\n")],
    );
    f.agent_edit("main.tex", "line 2\n", "LINE 2\n");
    common::pump_until(TIMEOUT, || f.hunk_starts("main.tex") == [2]);
    f.set_cursor(0, 0);
    // `f]` finds the bracket instead of starting `]h`.
    f.keys("f]");
    assert_eq!(f.cursor(), (0, 1));
    // `r]` replaces with a bracket at once, although Vim does not show the pending `r`.
    f.keys("lr]");
    assert!(f.text().starts_with("a]]]c\n"), "{:?}", f.text());
    assert_eq!(f.state.status.vim_command.text(), "", "nothing held");
    f.keys("2r[");
    assert!(f.text().starts_with("a][[c\n"), "{:?}", f.text());
    // `y<Space>` yanks a character (the space is `y`'s motion), then `a` appends: no agent.
    f.set_cursor(0, 0);
    f.keys("y<Space>a");
    assert_eq!(f.mode(), VimMode::Insert);
    assert!(f.actions().is_empty(), "{:?}", f.actions());
    f.keys("<Esc>");
    // An operator takes `]` as the start of its motion (no hunk navigation).
    f.set_cursor(1, 0);
    f.keys("d]");
    f.keys("<Esc>");
    assert_eq!(f.cursor().0, 1);
    assert_eq!(f.mode(), VimMode::Normal);
    // Visual mode: `]h` is not a binding there.
    f.keys("v]h<Esc>");
    assert_eq!(f.cursor().0, 1, "no navigation in visual mode");
    // The command line: typed into the command.
    f.keys(":]h");
    assert_eq!(f.vim_command(), ":]h");
    f.keys("<Esc>");
    assert_eq!(f.mode(), VimMode::Normal);
    assert_eq!(f.hunk_starts("main.tex"), [2]);
}

fn space_a_and_remapping() {
    let f = fixture("space-a", &main_files());
    f.with_hunks();
    f.set_cursor(0, 0);
    f.keys("<Space>a");
    common::pump_until(TIMEOUT, || f.actions() == ["agent-focus"]);
    assert_eq!(f.cursor(), (0, 0), "the space did not move the cursor");
    assert_eq!(f.text(), edited_main(), "`a` did not append");
    assert_eq!(f.mode(), VimMode::Normal);

    // `[vim.normal]` overrides, reloaded live: `gr` removed, `<Space>a` remapped, new sequences,
    // and an action that does not exist.
    let mut config = (*f.state.config.get()).clone();
    for (keys, action) in [
        ("gr", ""),
        ("<Space>a", "win.test-one"),
        ("<Space>b", "win.test-two"),
        ("<C-x>", "win.test-three"),
        ("zq", "win.no-such-action"),
    ] {
        config.vim.normal.insert(keys.into(), action.into());
    }
    f.state.set_config(Rc::new(config));
    f.actions.borrow_mut().clear();
    f.keys("<Space>a<Space>b<C-x>");
    common::pump_until(TIMEOUT, || {
        f.actions() == ["test-one", "test-two", "test-three"]
    });
    assert_eq!(f.text(), edited_main(), "Vim's Ctrl+X did not run");
    // `gr` is Vim's again: the hunk at the cursor stays.
    f.set_cursor(2, 0);
    f.keys("gr");
    f.keys("<Esc>");
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.hunk_starts("main.tex"), [2, 6, 10]);
    // An action that does not exist only shows a toast.
    f.keys("zq");
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.mode(), VimMode::Normal);
    // A bad key in the configuration skips that entry (with a toast); the others still work.
    let mut config = (*f.state.config.get()).clone();
    config
        .vim
        .normal
        .insert("<Leader>x".into(), "win.test-one".into());
    f.state.set_config(Rc::new(config));
    f.actions.borrow_mut().clear();
    f.keys("<Space>b");
    common::pump_until(TIMEOUT, || f.actions() == ["test-two"]);
}

fn bindings_follow_vim_toggle() {
    let f = fixture("toggle", &main_files());
    f.with_hunks();
    let layer = f.view().key_layer().unwrap();
    let with_vim = layer.len();
    assert!(
        with_vim >= 1,
        "the binding handler is registered while Vim is on"
    );
    // A held key is dropped when Vim goes off.
    f.set_cursor(0, 0);
    f.keys("]");
    f.state.set_vim_enabled(false);
    assert!(layer.len() < with_vim, "and removed while it is off");
    assert_eq!(f.state.status.vim_command.text(), "");
    let event = keys::event_for(
        &f.editor,
        gdk::Key::bracketright,
        gdk::ModifierType::empty(),
    );
    assert_eq!(keys::dispatch(f.view(), &event), glib::Propagation::Proceed);
    f.state.set_vim_enabled(true);
    assert_eq!(layer.len(), with_vim);
    f.keys("]h");
    f.wait_cursor_line(2);
    // A second tab has its own bindings; closing it removes them without warnings.
    std::fs::write(f.dir.join("b.tex"), "b\n").unwrap();
    let second = block_on(f.state.open_file(&f.dir.join("b.tex"))).unwrap();
    assert!(!second.view().key_layer().unwrap().is_empty());
    let before = common::logged_problems();
    f.state.close_editor(&second, true);
    drop(second);
    common::pump_for(Duration::from_millis(50));
    common::assert_no_new_problems(before);
    f.back_to_main();
    f.set_cursor(0, 0);
    f.keys("2]h");
    f.wait_cursor_line(6);
}

fn ex_commands_with_counts() {
    let f = fixture("ex", &main_files());
    f.with_hunks();
    f.set_cursor(0, 0);
    f.keys(":Hnext 2<CR>");
    f.wait_cursor_line(6);
    f.keys(":2Hprev<CR>");
    f.wait_cursor_line(10);
    f.keys(":Hprev 3<CR>");
    f.wait_cursor_line(10);
    // `:Hnext` without a count is the review controller's; it opens the next file.
    f.with_second_file();
    f.keys(":Hnext<CR>");
    f.wait_active("z.tex");
    f.back_to_main();
    f.set_cursor(2, 0);
    // `:Accept 2` accepts lines 2 and 6, then shows line 10.
    f.keys(":Accept 2<CR>");
    common::pump_until(TIMEOUT, || f.hunk_starts("main.tex") == [10]);
    f.wait_cursor_line(10);
    // `:Reject 5` rejects line 10, then (this file is done) the next file's hunk.
    f.keys(":Reject 5<CR>");
    common::pump_until(TIMEOUT, || {
        f.hunk_starts("main.tex").is_empty() && f.hunk_starts("z.tex").is_empty()
    });
    assert!(f.text().contains("line 10\n"));
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(f.dir.join("z.tex")).unwrap() == "zero\none\n"
    });
    // A malformed count is ours (a toast), not Vim's `:0`.
    f.back_to_main();
    f.set_cursor(5, 0);
    f.keys(":Hnext 0<CR>");
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.cursor().0, 5);
    assert_eq!(f.mode(), VimMode::Normal);
    // Other commands still reach Vim.
    f.keys(":3<CR>");
    assert_eq!(f.cursor().0, 2);
}

fn ex_agent_commands_reach_the_agent() {
    let f = fixture("ex-agent", &main_files());
    let agent = f.state.agent_controller().unwrap().clone();
    let prompts: Rc<RefCell<Vec<SentPrompt>>> = Rc::default();
    let errors: Rc<RefCell<Vec<String>>> = Rc::default();
    let p = prompts.clone();
    let prompt_hook = agent.connect_prompt(move |prompt| p.borrow_mut().push(prompt.clone()));
    let e = errors.clone();
    let event_hook = agent.connect_event(move |event| {
        if let AgentEvent::Error(message) = event {
            e.borrow_mut().push(message.clone());
        }
    });
    assert!(!f.state.agent.split.shows_sidebar());
    f.keys(":Agent explain the proof of Lemma 2<CR>");
    // No agent can start here (see the module docs): the prompt reaches the controller, which
    // either announces it or reports why it cannot run it.
    common::pump_until(TIMEOUT, || {
        !prompts.borrow().is_empty() || !errors.borrow().is_empty()
    });
    if let Some(prompt) = prompts.borrow().first() {
        assert_eq!(
            prompt.blocks.first(),
            Some(&lemma_agent::PromptBlock::Text(
                "explain the proof of Lemma 2".into()
            ))
        );
    }
    assert!(
        f.state.agent.split.shows_sidebar(),
        "the pane shows the turn"
    );
    assert_eq!(f.mode(), VimMode::Normal);
    // `:AgentStop!` stops the agent process.
    common::pump_for(Duration::from_millis(50));
    agent.set_state(AgentState::Idle);
    f.keys(":AgentStop!<CR>");
    common::pump_until(TIMEOUT, || agent.state() == AgentState::Stopped);
    // `:AgentStop` with nothing running only says so; `:Agent` alone focuses the pane.
    f.state.agent.split.set_show_sidebar(false);
    f.keys(":AgentStop<CR>");
    f.keys(":Agent<CR>");
    common::pump_until(TIMEOUT, || f.state.agent.split.shows_sidebar());
    // Every command was ours: Vim ran none of them.
    assert_eq!(f.text(), MAIN);
    agent.disconnect(prompt_hook);
    agent.disconnect(event_hook);
}

// ----- real input -----

mod real_input {
    //! See the module docs of this file.

    use super::*;
    use glib::translate::IntoGlib;
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Command, Stdio};

    /// A private `gtk4-broadwayd`, stopped (and its socket removed) on drop.
    struct Daemon {
        child: Child,
        display: u32,
        port: u16,
        socket: PathBuf,
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            #[cfg(unix)]
            if let Ok(pid) = i32::try_from(self.child.id()) {
                // SAFETY: kill(2) on our own child process, which has not been reaped yet.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    /// Starts a daemon on a free display and loopback port; `None` when `gtk4-broadwayd` is not
    /// installed.
    fn start_daemon() -> Option<Daemon> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let pid = std::process::id();
        for attempt in 0..8 {
            let display = 300 + (pid.wrapping_mul(7) + attempt * 37) % 600;
            // broadwayd listens on `broadway<display + 1>.socket`.
            let socket = runtime.join(format!("broadway{}.socket", display + 1));
            if socket.exists() {
                continue;
            }
            let port = TcpListener::bind(("127.0.0.1", 0))
                .and_then(|l| l.local_addr())
                .expect("a free loopback port")
                .port();
            let spawned = Command::new("gtk4-broadwayd")
                .args([
                    format!(":{display}"),
                    "--address".into(),
                    "127.0.0.1".into(),
                    "--port".into(),
                    port.to_string(),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let Ok(child) = spawned else {
                println!("    (gtk4-broadwayd is not installed: the real-input check is skipped)");
                return None;
            };
            let mut daemon = Daemon {
                child,
                display,
                port,
                socket,
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = daemon.child.try_wait() {
                    break;
                }
                if daemon.socket.exists() && TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(daemon);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        panic!("could not start a private gtk4-broadwayd");
    }

    /// The parent: a private daemon, and this binary again on it as the child.
    pub fn parent() {
        let Some(daemon) = start_daemon() else {
            return;
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("real_input_child")
            .env(CHILD_PORT_ENV, daemon.port.to_string())
            .env("GDK_BACKEND", "broadway")
            .env("BROADWAY_DISPLAY", format!(":{}", daemon.display))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the child");
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let readers = (
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = out.read_to_string(&mut s);
                s
            }),
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = err.read_to_string(&mut s);
                s
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let (out, err) = (readers.0.join().unwrap(), readers.1.join().unwrap());
        drop(daemon);
        let passed =
            status.is_some_and(|s| s.success()) && out.contains("test real_input_child ... ok");
        if !passed {
            println!("---- child stdout ----\n{out}\n---- child stderr ----\n{err}");
        }
        assert!(passed, "the real-input child failed ({status:?})");
    }

    /// A WebSocket client of the Broadway daemon speaking its browser protocol: each input
    /// message is a binary frame of big-endian `i32`s `[type, serial, time, args…]`; a key is
    /// `[keysym, modifier state]` and goes to the focused window.
    struct Browser {
        stream: TcpStream,
        time: i32,
    }

    const KEY_PRESS: i32 = 7;
    const KEY_RELEASE: i32 = 8;
    // GDK modifier bits, as the browser client sends them.
    const SHIFT: i32 = 1;
    const CONTROL: i32 = 4;
    const ALT: i32 = 8;

    impl Browser {
        fn connect(port: u16) -> Self {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            write!(
                stream,
                "GET /socket HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: broadway\r\n\
                 Origin: http://127.0.0.1:{port}\r\n\r\n"
            )
            .unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).expect("handshake");
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head);
            assert!(head.starts_with("HTTP/1.1 101"), "{head}");
            // The daemon streams the display to its browser: read and drop it.
            let mut reader = stream.try_clone().unwrap();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 1 << 16];
                while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
            });
            Self { stream, time: 1000 }
        }

        fn send(&mut self, words: &[i32]) {
            let payload: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
            let mask = [0x5a, 0x17, 0xc3, 0x08];
            let mut frame = vec![0x82, 0x80 | u8::try_from(payload.len()).unwrap()];
            frame.extend_from_slice(&mask);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.stream.write_all(&frame).unwrap();
        }

        /// Presses and releases `key` with the modifier bits `mods`.
        fn key(&mut self, key: gdk::Key, mods: i32) {
            let keysym = i32::try_from(key.into_glib()).unwrap();
            self.time += 20;
            self.send(&[KEY_PRESS, 0, self.time, keysym, mods]);
            self.send(&[KEY_RELEASE, 0, self.time + 5, keysym, mods]);
        }

        /// Types `notation` (Vim notation) key by key, running the main loop after each.
        fn keys(&mut self, notation: &str) {
            if notation.is_empty() {
                return;
            }
            for name in keys::parse_keys(notation).unwrap() {
                let (key, state) = key_of(&name);
                let mut mods = 0;
                if state.contains(gdk::ModifierType::CONTROL_MASK) {
                    mods |= CONTROL;
                }
                if state.contains(gdk::ModifierType::ALT_MASK) {
                    mods |= ALT;
                }
                if key.is_upper() && key.to_lower() != key {
                    mods |= SHIFT;
                }
                self.key(key, mods);
                common::pump_for(Duration::from_millis(15));
            }
        }
    }

    /// What the probe handler saw: the key, the mode, and the buffer text at that moment.
    type Seen = Rc<RefCell<Vec<(gdk::Key, VimMode, String)>>>;

    /// The child: the acceptance scenarios on real key events.
    pub fn child() {
        let port: u16 = std::env::var(CHILD_PORT_ENV).unwrap().parse().unwrap();
        let f = fixture("real-input", &main_files());
        f.with_hunks();
        f.with_second_file();
        f.back_to_main();
        // The only window on this private display: it is the active one, so it gets the keys.
        f.wait("the window to be active", || f.view().has_focus());

        // A probe above every other handler; it stops Tab in insert mode while `stop_tab`.
        let seen: Seen = Rc::default();
        let stop_tab = Rc::new(Cell::new(true));
        let (log, stop, ed) = (seen.clone(), stop_tab.clone(), Rc::downgrade(&f.editor));
        let probe = keys::register(
            f.view(),
            i32::MAX,
            move |event: &KeyEvent, mode: VimMode| {
                let Some(editor) = ed.upgrade() else {
                    return glib::Propagation::Proceed;
                };
                log.borrow_mut()
                    .push((event.keyval, mode, editor.document().text()));
                if event.keyval == gdk::Key::Tab && mode == VimMode::Insert && stop.get() {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            },
        )
        .unwrap();
        // Vim's pending command after every key it handles.
        let vim_commands: Rc<RefCell<Vec<String>>> = Rc::default();
        let (log, ed) = (vim_commands.clone(), Rc::downgrade(&f.editor));
        f.editor
            .vim()
            .unwrap()
            .connect_notify_local(Some("command-text"), move |_, _| {
                if let Some(status) = ed.upgrade().and_then(|e| e.vim_status()) {
                    log.borrow_mut().push(status.command);
                }
            });
        let mut browser = Browser::connect(port);
        let last_seen = || seen.borrow().last().cloned().expect("the probe saw a key");

        // 1. The layer sees a key before Vim: `x` arrives while the character is still there,
        //    and Vim deletes it afterwards.
        f.set_cursor(0, 0);
        let original = f.text();
        browser.keys("x");
        f.wait("Vim to delete the character", || f.text() != original);
        assert_eq!(
            last_seen(),
            (gdk::Key::x, VimMode::Normal, original.clone())
        );
        assert!(f.text().starts_with("ine 0\n"));
        browser.keys("u");
        f.wait("u to restore the text", || f.text() == original);

        // 2. `]h` / `[h` in normal mode move between hunks, and Vim never sees their keys.
        vim_commands.borrow_mut().clear();
        browser.keys("]h");
        f.wait_cursor_line(2);
        browser.keys("]h");
        f.wait_cursor_line(6);
        browser.keys("[h");
        f.wait_cursor_line(2);
        assert!(
            vim_commands.borrow().iter().all(String::is_empty),
            "Vim saw a key of ]h/[h: {:?}",
            vim_commands.borrow()
        );
        assert_eq!(f.text(), original);
        // With a count, across files: 3 hunks on from line 2 is z.tex's.
        browser.keys("3]h");
        f.wait_active("z.tex");
        f.back_to_main();

        // 3. `]h` in insert mode is typed.
        f.set_cursor(0, 0);
        browser.keys("i]h");
        f.wait("]h to be typed", || f.text().starts_with("]hline 0\n"));
        assert_eq!(f.cursor(), (0, 2));

        // 4. Tab in insert mode reaches the layer before Vim indents: stopped there, nothing
        //    happens; let through, Vim and the view indent.
        let typed = f.text();
        browser.keys("<Tab>");
        common::pump_for(Duration::from_millis(50));
        assert_eq!(last_seen(), (gdk::Key::Tab, VimMode::Insert, typed.clone()));
        assert_eq!(f.text(), typed, "the stopped Tab changed nothing");
        stop_tab.set(false);
        browser.keys("<Tab>");
        f.wait("the Tab to indent", || f.text() != typed);
        browser.keys("<Esc>");
        f.wait("normal mode after Esc", || f.mode() == VimMode::Normal);
        for _ in 0..4 {
            if f.text() == original {
                break;
            }
            browser.keys("u");
            common::pump_for(Duration::from_millis(30));
        }
        assert_eq!(f.text(), original);

        // 5. The chords run in every mode, before the layer (which never sees them), and keep
        //    the mode and the text.
        for (enter, mode) in [
            ("", VimMode::Normal),
            ("i", VimMode::Insert),
            ("v", VimMode::Visual),
        ] {
            browser.keys("<Esc>");
            f.wait("normal mode after Esc", || f.mode() == VimMode::Normal);
            f.set_cursor(0, 0);
            browser.keys(enter);
            f.wait("the mode to enter", || f.mode() == mode);
            seen.borrow_mut().clear();
            f.actions.borrow_mut().clear();
            browser.keys("<M-]>");
            f.wait_cursor_line(2);
            browser.keys("<C-k>");
            f.wait("Ctrl+K to run win.agent-inline-edit", || {
                f.actions() == ["agent-inline-edit"]
            });
            let shown = f.state.agent.split.shows_sidebar();
            // (In this narrow window the pane is an overlay that keeps the focus while it is
            // shown; F10 is a window accelerator, so the second press works from there.)
            browser.keys("<F10>");
            f.wait("the agent pane toggled", || {
                f.state.agent.split.shows_sidebar() != shown
            });
            browser.keys("<F10>");
            f.wait("the agent pane toggled back", || {
                f.state.agent.split.shows_sidebar() == shown
            });
            f.back_to_main();
            assert_eq!(f.mode(), mode, "{mode:?} is kept");
            assert_eq!(f.text(), original);
            assert!(
                seen.borrow().is_empty(),
                "the layer saw a chord: {:?}",
                seen.borrow()
            );
        }
        browser.keys("<Esc>");
        f.wait("normal mode after Esc", || f.mode() == VimMode::Normal);

        // 6. `ga` accepts the hunk at the cursor; `<Space>a` runs win.agent-focus; the held `g`
        //    and `<Space>` go to Vim as the real events (`gg`, `<Space>l`).
        f.set_cursor(2, 0);
        browser.keys("ga");
        f.wait("ga to accept the hunk", || {
            f.hunk_starts("main.tex") == [6, 10]
        });
        f.wait_cursor_line(6);
        f.actions.borrow_mut().clear();
        browser.keys("<Space>a");
        f.wait("<Space>a to run win.agent-focus", || {
            f.actions() == ["agent-focus"]
        });
        browser.keys("gg");
        f.wait("gg to reach the top", || f.cursor() == (0, 0));
        browser.keys("<Space>l");
        f.wait("<Space>l to move two columns", || f.cursor() == (0, 2));
        assert_eq!(f.mode(), VimMode::Normal);

        // 7. With Vim on, Ctrl+W, Ctrl+O and Ctrl+N in the editor are Vim's in every mode
        //    (insert mode: Ctrl-W deletes the word before the cursor), not "close the tab",
        //    "open a file" and "new file"; with the focus elsewhere, or with Vim off, they are
        //    those actions again.
        let (closed, opened) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let created = Rc::new(Cell::new(0));
        let close_tab = gio::SimpleAction::new("close-tab", None);
        let count = closed.clone();
        close_tab.connect_activate(move |_, _| count.set(count.get() + 1));
        f.state.window.add_action(&close_tab);
        let open = gio::SimpleAction::new("open", None);
        let count = opened.clone();
        open.connect_activate(move |_, _| count.set(count.get() + 1));
        f.state.app.add_action(&open);
        let new_file = gio::SimpleAction::new("new-file", None);
        let count = created.clone();
        new_file.connect_activate(move |_, _| count.set(count.get() + 1));
        f.state.window.add_action(&new_file);
        let tabs = f.state.editors().len();
        f.set_cursor(1, 0);
        browser.keys("A foo bar");
        f.wait("the words to be typed", || {
            f.text().contains("line 1 foo bar\n")
        });
        browser.keys("<C-w>");
        f.wait("Ctrl-W to delete a word", || {
            !f.text().contains("line 1 foo bar\n")
        });
        assert!(f.text().contains("line 1 foo \n"), "{:?}", f.text());
        browser.keys("<C-o>");
        browser.keys("<C-n>");
        common::pump_for(Duration::from_millis(50));
        browser.keys("<Esc>");
        f.wait("normal mode after Esc", || f.mode() == VimMode::Normal);
        browser.keys("<C-w>");
        browser.keys("<C-o>");
        browser.keys("<C-n>");
        common::pump_for(Duration::from_millis(50));
        assert_eq!(
            (closed.get(), opened.get(), created.get()),
            (0, 0, 0),
            "Ctrl+W/Ctrl+O/Ctrl+N reached the accelerators while Vim had the keys"
        );
        assert_eq!(f.state.editors().len(), tabs, "no tab closed");
        // The focus outside the editor: the accelerators.
        GtkWindowExt::set_focus(&f.state.window, None::<&gtk::Widget>);
        common::pump_for(Duration::from_millis(30));
        browser.keys("<C-w>");
        f.wait("Ctrl+W outside the editor to close the tab", || {
            closed.get() == 1
        });
        browser.keys("<C-o>");
        f.wait("Ctrl+O outside the editor to open", || opened.get() == 1);
        browser.keys("<C-n>");
        f.wait("Ctrl+N outside the editor for a new file", || {
            created.get() == 1
        });
        // Vim off: the accelerators in the editor too.
        f.state.set_vim_enabled(false);
        f.view().grab_focus();
        f.wait("the editor to have the focus", || f.view().has_focus());
        browser.keys("<C-w>");
        f.wait("Ctrl+W without Vim to close the tab", || closed.get() == 2);
        browser.keys("<C-o>");
        f.wait("Ctrl+O without Vim to open", || opened.get() == 2);
        browser.keys("<C-n>");
        f.wait("Ctrl+N without Vim for a new file", || created.get() == 2);

        keys::unregister(f.view(), probe);
    }
}
