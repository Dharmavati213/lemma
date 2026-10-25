//! Vim mode on a real window (WP0.2 acceptance 5): keys are typed into GtkSourceView's
//! `VimIMContext` with `gtk_im_context_filter_key`, which builds the same key events a keyboard
//! produces (keycodes come from the display's keymap). That is exactly what the capture-phase key
//! controller does with a real key press, so the command bar (`:w⏎`) behaves as it does
//! interactively. A key Vim does not consume would continue to the `GtkTextView`; GTK has no API
//! to inject events, so [`Fixture::type_keys`] emulates that step for printable characters.
//!
//! Lemma's `:s` (`vim::substitute`) is checked against GtkSourceView's own on a bare view with
//! Vim and none of Lemma's handlers ([`Bare`]): the same keys give the same text, cursor, jump
//! list and search pattern. `LEMMA_TEST_SEED` / `LEMMA_TEST_OPS` (default 60) change its random
//! cases.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_vim`. The `:s` timings
//! run in release: `scripts/broadway.sh cargo test --release -p lemma-app --test gtk_vim --
//! --ignored perf`.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::editor::{Editor, VimMode, vim};
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Set in a child process: the joins that must never hang run there (see
/// [`joins_near_the_end_return`]).
const JOIN_CHILD_ENV: &str = "LEMMA_GTK_VIM_JOIN_CHILD";

fn main() -> ExitCode {
    if std::env::var_os(JOIN_CHILD_ENV).is_some() {
        return common::run(&[("join_child", join_child)]);
    }
    let mut tests: Vec<common::Test> = vec![
        ("typing_in_insert_mode", typing_in_insert_mode),
        ("status_follows_the_mode", status_follows_the_mode),
        ("colon_w_saves_through_the_write_signal", colon_w_saves),
        (
            "colon_wq_saves_and_closes_the_tab",
            colon_wq_saves_and_closes,
        ),
        ("colon_wa_saves_every_document", colon_wa_saves_all),
        ("colon_w_path_writes_a_copy", colon_w_path),
        ("colon_saveas_saves_under_another_name", colon_saveas),
        ("unknown_commands_reach_execute_command", unknown_commands),
        ("colon_q_closes_and_asks_when_dirty", colon_q),
        ("colon_e_opens_a_file", colon_e),
        ("colon_qa_bang_closes_the_window", colon_qa_bang),
        ("vim_can_be_turned_off", vim_can_be_turned_off),
        ("vim_is_off_by_default_and_turns_on", vim_off_by_default),
        ("joins_near_the_end_return", joins_near_the_end_return),
        (
            "colon_s_does_what_gtksourceview_does",
            colon_s_matches_gtksourceview,
        ),
        (
            "colon_s_is_one_edit_and_one_undo_step",
            colon_s_is_one_undo_step,
        ),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_vim_substitute_range_10k", perf_substitute_range));
        tests.push(("perf_vim_substitute_whole_10k", perf_substitute_whole));
        tests.push(("perf_vim_substitute_line_large_tex", perf_substitute_line));
        tests.push(("perf_vim_substitute_large_tex", perf_substitute_large));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(10);

/// A window with one document open, as the app builds it.
struct Fixture {
    state: Rc<WindowState>,
    editor: Rc<Editor>,
    path: std::path::PathBuf,
    _app: adw::Application,
}

fn fixture(name: &str, text: &str) -> Fixture {
    fixture_with(name, text, true)
}

fn fixture_with(name: &str, text: &str, vim: bool) -> Fixture {
    let dir = common::temp_dir(name);
    let path = dir.join("main.tex");
    std::fs::write(&path, text).unwrap();
    let app = common::application();
    let mut config = Config::default();
    config.build.on_save = false;
    // Vim is off by default since M5; most of these tests are about it.
    config.editor.vim = vim;
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, false);
    state.window.present();
    let editor = glib::MainContext::default()
        .block_on(state.open_file(&path))
        .expect("open");
    common::pump_until(TIMEOUT, || {
        state.window.is_mapped() && editor.view().has_focus()
    });
    Fixture {
        state,
        editor,
        path,
        _app: app,
    }
}

impl Fixture {
    fn im(&self) -> sourceview5::VimIMContext {
        self.editor.vim().expect("Vim is on")
    }

    /// Types `keys`: characters, plus `\n` for Return and `\x1b` for Escape.
    fn type_keys(&self, keys: &str) {
        self.type_keys_with(keys, false);
    }

    /// [`Fixture::type_keys`], each key first passing Lemma's key guard as a real key press does
    /// (`Editor::vim_blocks_key`: a key it stops never reaches Vim).
    fn type_guarded(&self, keys: &str) {
        self.type_keys_with(keys, true);
    }

    fn type_keys_with(&self, keys: &str, guarded: bool) {
        type_keys_into(&self.state, &self.editor, keys, guarded);
    }

    fn text(&self) -> String {
        self.editor.document().text()
    }

    fn disk(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }
}

/// [`Fixture::type_keys_with`] into `editor` (another tab of the fixture's window).
fn type_keys_into(state: &WindowState, editor: &Editor, keys: &str, guarded: bool) {
    let im = editor.vim().expect("Vim is on");
    let surface = state.window.surface().expect("realized window");
    feed_keys(
        &im,
        &surface,
        keys,
        |keyval, mods| guarded && editor.vim_blocks_key(keyval, mods),
        // What the text view does with a key Vim lets through (insert mode).
        |c| {
            editor
                .buffer()
                .insert_interactive_at_cursor(&c.to_string(), true);
        },
    );
}

/// Gives Vim's `im` on `surface` a press and a release of each key of `keys`: characters,
/// `\n` for Return, `\x1b` for Escape and `\x0f` for Ctrl+O. `skip` may stop a key before Vim
/// sees it; `through` gets the characters Vim does not consume.
fn feed_keys(
    im: &sourceview5::VimIMContext,
    surface: &gdk::Surface,
    keys: &str,
    mut skip: impl FnMut(gdk::Key, gdk::ModifierType) -> bool,
    mut through: impl FnMut(char),
) {
    let display = surface.display();
    let keyboard = display
        .default_seat()
        .and_then(|s| s.keyboard())
        .expect("a keyboard device");
    for c in keys.chars() {
        let (keyval, control) = match c {
            '\n' => (gdk::Key::Return, false),
            '\x1b' => (gdk::Key::Escape, false),
            '\x0f' => (gdk::Key::o, true),
            // `gdk::Key` has no safe constructor from a keyval; it is a `u32` GValue.
            c => (
                gdk::unicode_to_keyval(u32::from(c))
                    .to_value()
                    .get::<gdk::Key>()
                    .expect("a keyval"),
                false,
            ),
        };
        let key = display
            .map_keyval(keyval)
            .and_then(|keys| keys.into_iter().min_by_key(|k| k.level()))
            .unwrap_or_else(|| panic!("no keycode for {c:?}"));
        let mut state = if key.level() > 0 {
            gdk::ModifierType::SHIFT_MASK
        } else {
            gdk::ModifierType::empty()
        };
        if control {
            state |= gdk::ModifierType::CONTROL_MASK;
        }
        if skip(keyval, state) {
            common::pump_for(Duration::from_millis(2));
            continue;
        }
        let consumed = im.filter_key(
            true,
            surface,
            &keyboard,
            0,
            key.keycode(),
            state,
            key.group(),
        );
        im.filter_key(
            false,
            surface,
            &keyboard,
            0,
            key.keycode(),
            state,
            key.group(),
        );
        if !consumed && c != '\x1b' && !control {
            through(c);
        }
        common::pump_for(Duration::from_millis(2));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
    }
}

fn typing_in_insert_mode() {
    let f = fixture("insert", "world\n");
    f.type_keys("ihello \x1b");
    assert_eq!(f.text(), "hello world\n");
    f.type_keys("A!\x1b");
    assert_eq!(f.text(), "hello world!\n");
    f.type_keys("u");
    assert_eq!(f.text(), "hello world\n", "u undoes");
}

fn status_follows_the_mode() {
    let f = fixture("status", "x\n");
    let mode = || f.editor.vim_status().expect("vim on").mode;
    assert_eq!(mode(), VimMode::Normal);
    assert_eq!(f.state.status.mode.text(), "NORMAL");
    f.type_keys("i");
    assert_eq!(mode(), VimMode::Insert);
    assert_eq!(f.state.status.mode.text(), "INSERT");
    f.type_keys("\x1bv");
    assert_eq!(mode(), VimMode::Visual);
    f.type_keys("\x1bV");
    assert_eq!(mode(), VimMode::VisualLine);
    f.type_keys("\x1b2d");
    let status = f.editor.vim_status().unwrap();
    assert_eq!(
        (status.mode, status.command.as_str()),
        (VimMode::Normal, "2d")
    );
    assert_eq!(f.state.status.vim_command.text(), "2d");
    f.type_keys("\x1b:wri");
    let status = f.editor.vim_status().unwrap();
    assert_eq!(
        (status.mode, status.command.as_str()),
        (VimMode::CommandLine, ":wri")
    );
    assert_eq!(f.state.status.vim_command.text(), ":wri");
    f.type_keys("\x1b");
    assert_eq!(mode(), VimMode::Normal);
}

fn colon_w_saves() {
    let f = fixture("w", "one\n");
    f.type_keys("Otwo\x1b");
    assert_eq!(f.text(), "two\none\n");
    assert!(f.editor.document().is_dirty());
    let (writes, commands) = (Rc::new(Cell::new(0)), Rc::new(RefCell::new(Vec::new())));
    let w = writes.clone();
    f.im().connect_write(move |_, _, _| w.set(w.get() + 1));
    let c = commands.clone();
    f.im().connect_execute_command(move |_, cmd| {
        c.borrow_mut().push(cmd.to_owned());
        false
    });
    f.type_keys(":w\n");
    // Interactively, `:w` is offered to `execute-command` first and then, not handled there,
    // emits `write` (unlike `execute_command(":w")` from code, which emits neither).
    assert_eq!(writes.get(), 1, "the write signal fired");
    assert_eq!(*commands.borrow(), [":w"]);
    common::pump_until(TIMEOUT, || !f.editor.document().is_dirty());
    assert_eq!(f.disk(), "two\none\n");
    assert_eq!(f.text(), "two\none\n", ":w must not change the text");
}

fn colon_wq_saves_and_closes() {
    let f = fixture("wq", "one\n");
    f.type_keys("Azero\x1b");
    f.type_keys(":wq\n");
    common::pump_until(TIMEOUT, || f.disk() == "onezero\n");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    assert!(
        f.state.workspace.is_empty(),
        "the document left the workspace"
    );
}

fn colon_wa_saves_all() {
    let f = fixture("wa", "one\n");
    let other = f.path.with_file_name("other.tex");
    std::fs::write(&other, "x\n").unwrap();
    let second = glib::MainContext::default()
        .block_on(f.state.open_file(&other))
        .unwrap();
    second.buffer().insert_at_cursor("y");
    f.state.select_editor(&f.editor);
    f.type_keys("Ay\x1b:wa\n");
    common::pump_until(TIMEOUT, || {
        f.disk() == "oney\n" && std::fs::read_to_string(&other).unwrap() == "yx\n"
    });
    assert_eq!(f.state.tab_view.n_pages(), 2, ":wa keeps the tabs");
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn active_path(state: &WindowState) -> Option<PathBuf> {
    state
        .active_editor()
        .map(|e| e.document().path().to_path_buf())
}

fn colon_w_path() {
    let f = fixture("w-path", "one\n");
    let dir = normalize(f.path.parent().unwrap());
    let copy = dir.join("copy.tex");
    f.type_keys("Atwo\x1b");
    // A copy: the document stays the file it was, with its unsaved changes (relative to the
    // project root, as `:e`).
    f.type_keys(":w copy.tex\n");
    // Written, and no document of its own stays.
    common::pump_until(TIMEOUT, || {
        read(&copy) == "onetwo\n" && f.state.workspace.get(&copy).is_none()
    });
    assert_eq!(f.disk(), "one\n");
    assert!(f.editor.document().is_dirty());
    assert_eq!(f.state.tab_view.n_pages(), 1);
    assert_eq!(active_path(&f.state), Some(normalize(&f.path)));
    // A command, an append or `%` is not a file name to write (GtkSourceView passes them on as
    // typed).
    f.type_keys(":w !cat\n:w >> copy.tex\n:w %.bak\n:sav %.bak\n");
    common::pump_for(Duration::from_millis(200));
    for junk in ["!cat", ">> copy.tex", "%.bak"] {
        assert!(!dir.join(junk).exists(), "{junk}");
    }
    // (A `!` later in the name is a file name.)
    f.type_keys(":w a!b.tex\n");
    common::pump_until(TIMEOUT, || read(&dir.join("a!b.tex")) == "onetwo\n");
    assert_eq!(read(&copy), "onetwo\n");
    assert_eq!(active_path(&f.state), Some(normalize(&f.path)));
    // A file that exists is replaced only with `!`.
    f.type_keys("Athree\x1b:w copy.tex\n");
    common::pump_for(Duration::from_millis(200));
    assert_eq!(read(&copy), "onetwo\n");
    f.type_keys(":w! copy.tex\n");
    common::pump_until(TIMEOUT, || read(&copy) == "onetwothree\n");
    std::fs::create_dir(dir.join("sub")).unwrap();
    f.type_keys(":w sub/c.tex\n");
    common::pump_until(TIMEOUT, || read(&dir.join("sub/c.tex")) == "onetwothree\n");
    // `:w` with its own name saves as before.
    f.type_keys(":w main.tex\n");
    common::pump_until(TIMEOUT, || !f.editor.document().is_dirty());
    assert_eq!(f.disk(), "onetwothree\n");

    // `:wq path`: the copy, then `:q`, which asks about unsaved changes; `:wq! path` drops
    // them.
    f.type_keys("Afour\x1b:wq last.tex\n");
    answer(&f, "cancel");
    assert_eq!(read(&dir.join("last.tex")), "onetwothreefour\n");
    assert_eq!(f.state.tab_view.n_pages(), 1);
    f.type_keys(":wq! last2.tex\n");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    assert_eq!(read(&dir.join("last2.tex")), "onetwothreefour\n");
    assert_eq!(f.disk(), "onetwothree\n");
}

fn colon_saveas() {
    let f = fixture("saveas", "one\n");
    let dir = normalize(f.path.parent().unwrap());
    f.type_keys("Atwo\x1b");
    // A file name is required.
    f.type_keys(":sav\n");
    common::pump_for(Duration::from_millis(100));
    assert_eq!(active_path(&f.state), Some(normalize(&f.path)));
    // Save As: the new file in the tab, the old one as it was on disk.
    f.type_keys(":sav new.tex\n");
    common::pump_until(TIMEOUT, || {
        active_path(&f.state) == Some(dir.join("new.tex")) && f.state.tab_view.n_pages() == 1
    });
    assert_eq!(read(&dir.join("new.tex")), "onetwo\n");
    assert_eq!(f.disk(), "one\n");
    // Vim goes on in the new tab: `:saveas` refuses a file that exists, `:saveas!` replaces it.
    let new = f.state.active_editor().unwrap();
    assert!(new.vim().is_some());
    type_keys_into(&f.state, &new, "Athree\x1b:saveas main.tex\n", false);
    common::pump_for(Duration::from_millis(200));
    assert_eq!(active_path(&f.state), Some(dir.join("new.tex")));
    assert_eq!(f.disk(), "one\n");
    type_keys_into(&f.state, &new, ":saveas! main.tex\n", false);
    common::pump_until(TIMEOUT, || {
        active_path(&f.state) == Some(normalize(&f.path)) && f.state.tab_view.n_pages() == 1
    });
    assert_eq!(f.disk(), "onetwothree\n");
    assert_eq!(read(&dir.join("new.tex")), "onetwo\n");
}

fn unknown_commands() {
    let f = fixture("unknown", "a\n");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let sink = seen.clone();
    f.im().connect_execute_command(move |_, cmd| {
        sink.borrow_mut().push(cmd.to_owned());
        false
    });
    // (`:Accept` and the other review commands are handled by the review controller.)
    f.type_keys(":Frobnicate\n");
    // Built-in commands are offered first too, and run when not handled (a substitution with a
    // letter for its separator is GtkSourceView's own).
    f.type_keys(":sxaxbx\n");
    assert_eq!(*seen.borrow(), [":Frobnicate", ":sxaxbx"]);
    assert_eq!(f.text(), "b\n");
    // Lemma runs the usual ones itself (`vim::substitute`).
    f.type_keys(":s/b/c/\n");
    assert_eq!(*seen.borrow(), [":Frobnicate", ":sxaxbx"]);
    assert_eq!(f.text(), "c\n");
}

/// M5: `[editor] vim` defaults to false; the toggle still turns Vim on (and off again).
fn vim_off_by_default() {
    assert!(!Config::default().editor.vim);
    let f = fixture_with("default", "a\n", Config::default().editor.vim);
    assert!(!f.state.vim_enabled());
    assert!(f.editor.vim().is_none());
    assert!(!f.state.status.mode.is_visible());
    WidgetExt::activate_action(&f.state.window, "win.toggle-vim", None).unwrap();
    assert!(f.state.vim_enabled());
    assert!(f.editor.vim().is_some());
    assert!(f.state.status.mode.is_visible());
    f.type_keys("ix\x1b");
    assert_eq!(f.text(), "xa\n");
    WidgetExt::activate_action(&f.state.window, "win.toggle-vim", None).unwrap();
    assert!(!f.state.vim_enabled() && f.editor.vim().is_none());
}

fn vim_can_be_turned_off() {
    let f = fixture("off", "a\n");
    WidgetExt::activate_action(&f.state.window, "win.toggle-vim", None).unwrap();
    assert!(!f.state.vim_enabled());
    let action_state = || {
        f.state
            .window
            .lookup_action("toggle-vim")
            .and_then(|a| a.state())
            .and_then(|s| s.get::<bool>())
    };
    assert_eq!(action_state(), Some(false));
    assert!(f.editor.vim().is_none());
    assert!(!f.state.status.mode.is_visible());
    assert!(!f.editor.view().overwrites());
    WidgetExt::activate_action(&f.state.window, "win.toggle-vim", None).unwrap();
    assert!(f.editor.vim().is_some());
    assert!(f.state.status.mode.is_visible());
    f.state.set_vim_enabled(false);
    assert_eq!(
        action_state(),
        Some(false),
        "the menu state follows set_vim_enabled"
    );
}

fn answer(f: &Fixture, response: &str) {
    common::pump_until(TIMEOUT, || f.state.window.visible_dialog().is_some());
    let alert = f
        .state
        .window
        .visible_dialog()
        .and_then(|d| d.downcast::<adw::AlertDialog>().ok())
        .expect("an alert dialog");
    alert.emit_by_name::<()>("response", &[&response]);
    alert.force_close();
    common::pump_for(Duration::from_millis(50));
}

fn colon_q() {
    let f = fixture("q", "a\n");
    f.type_keys("Ab\x1b:q\n");
    answer(&f, "cancel");
    assert_eq!(
        f.state.tab_view.n_pages(),
        1,
        "a dirty tab asks before closing"
    );
    f.type_keys(":q!\n");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 0);
    assert_eq!(f.disk(), "a\n", ":q! discards");
    assert!(f.state.window.visible_dialog().is_none());
}

fn colon_e() {
    let f = fixture("e", "a\n");
    std::fs::write(f.path.with_file_name("other.tex"), "other\n").unwrap();
    f.type_keys(":e other.tex\n");
    common::pump_until(TIMEOUT, || f.state.tab_view.n_pages() == 2);
    let active = f.state.active_editor().unwrap();
    assert_eq!(active.document().text(), "other\n");
}

fn colon_qa_bang() {
    let f = fixture("qa", "a\n");
    f.type_keys("Ab\x1b:qa!\n");
    common::pump_until(TIMEOUT, || !f.state.window.is_visible());
    assert_eq!(f.disk(), "a\n");
}

/// Joins whose lines reach the whitespace at the end of the buffer (GtkSourceView's join would
/// never return) are refused, whether counted (`2J`), repeated (`.`), visual or ex (`:%j`);
/// joins that stay before it still work, and `.` repeats a join only while the join is Vim's
/// last change. Runs in a child process that is killed after 30 s (a hang).
fn joins_near_the_end_return() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .env(JOIN_CHILD_ENV, "1")
        .spawn()
        .expect("spawn the child");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the child") {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = status.expect("a join near the end of the buffer hung (child killed after 30 s)");
    assert!(status.success(), "the join child failed: {status}");
}

fn join_child() {
    // (text, keys, expected text); GtkSourceView joins one line more than Vim for a count.
    let cases: &[(&str, &str, &str)] = &[
        // Counted joins that reach the empty last line: refused.
        ("a\nb\nc\nd\n", "jj2J", "a\nb\nc\nd\n"),
        ("a\nb\nc\nd\ne\nf\n", "3j3J", "a\nb\nc\nd\ne\nf\n"),
        ("a\nb\nc\nd\n", "j9J", "a\nb\nc\nd\n"),
        // Counted joins before it still work.
        ("a\nb\nc\nd\n", "2J", "a b c\nd\n"),
        ("a\nb\nc\nd\n", "j2J", "a\nb c d\n"),
        // `.` repeats a join until it would reach the end.
        ("a\nb\nc\nd\n", "Jj.", "a b\nc d\n"),
        ("a\nb\nc\nd\n", "Jj..", "a b\nc d\n"),
        ("a\nb\nc\nd\n", "J.", "a b c\nd\n"),
        ("a\nb\nc\nd\n", "J..", "a b c d\n"),
        ("a\nb\nc\nd\n", "J...", "a b c d\n"),
        // An undo leaves the join Vim's last change.
        ("a\nb\nc\n", "Jujj.", "a\nb\nc\n"),
        // Another change replaces it: `.` repeats `x`, not the join.
        ("a\nbc\n", "J0x.", "bc\n"),
        // A visual join of the last non-blank line.
        ("a\nb\nc\n", "GkVJ", "a\nb\nc\n"),
        // Ex joins: a range or count that reaches the end is refused.
        ("a\nb\nc\nd\n", ":%j\n", "a\nb\nc\nd\n"),
        ("a\nb\nc\nd\n", "j:j 3\n", "a\nb\nc\nd\n"),
        ("a\nb\nc\nd\n", ":2,$j\n", "a\nb\nc\nd\n"),
    ];
    let mut failures = Vec::new();
    for (text, keys, expected) in cases {
        let f = fixture("join-child", text);
        println!("  {text:?}: typing {keys:?}");
        f.type_guarded(keys);
        f.type_guarded("\x1b");
        let result = f.text();
        println!("    -> {result:?}");
        if result != *expected {
            failures.push(format!(
                "{text:?} + {keys:?}: {result:?}, expected {expected:?}"
            ));
        }
    }
    // Joins before the end reach GtkSourceView, which (5.20) implements neither `:j` nor a
    // visual `J`: they only have to return.
    for (text, keys) in [
        ("a\nb\nc\nd\ne\n", ":j\n"),
        ("a\nb\nc\nd\ne\n", ":j\nj."),
        ("a\nb\nc\nd\n", "ggVjJ"),
    ] {
        let f = fixture("join-child-more", text);
        f.type_guarded(keys);
        println!("  {text:?} + {keys:?} -> {:?}", f.text());
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

// ----- `:s` -----

/// A bare GtkSourceView view with Vim and none of Lemma's handlers: what GtkSourceView does
/// with the keys it is given.
struct Bare {
    window: gtk::Window,
    view: sourceview5::View,
    im: sourceview5::VimIMContext,
}

impl Bare {
    fn new() -> Self {
        let buffer = sourceview5::Buffer::new(None);
        // As Lemma's buffers (`document::new_buffer`).
        sourceview5::prelude::BufferExt::set_implicit_trailing_newline(&buffer, false);
        let view = sourceview5::View::with_buffer(&buffer);
        let window = gtk::Window::builder()
            .default_width(600)
            .default_height(400)
            .child(&view)
            .build();
        let im = sourceview5::VimIMContext::new();
        let keys = gtk::EventControllerKey::new();
        keys.set_im_context(Some(&im));
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        view.add_controller(keys);
        im.set_client_widget(Some(&view));
        window.present();
        common::pump_until(TIMEOUT, || window.is_mapped());
        Self { window, view, im }
    }

    fn type_keys(&self, keys: &str) {
        let surface = self.window.surface().expect("realized window");
        let buffer = self.view.buffer();
        feed_keys(
            &self.im,
            &surface,
            keys,
            |_, _| false,
            |c| {
                buffer.insert_interactive_at_cursor(&c.to_string(), true);
            },
        );
    }
}

impl Drop for Bare {
    fn drop(&mut self) {
        self.window.destroy();
    }
}

/// Replaces the text of `buffer` (not an undo step) and puts the cursor at character `cursor`.
fn reset(buffer: &impl IsA<gtk::TextBuffer>, text: &str, cursor: i32) {
    let buffer = buffer.as_ref();
    buffer.begin_irreversible_action();
    buffer.set_text(text);
    buffer.end_irreversible_action();
    buffer.place_cursor(&buffer.iter_at_offset(cursor));
}

/// The text, the cursor and the selection bound (character offsets) of `buffer`.
fn look(buffer: &impl IsA<gtk::TextBuffer>) -> (String, i32, i32) {
    let buffer = buffer.as_ref();
    let (start, end) = buffer.bounds();
    (
        buffer.text(&start, &end, true).to_string(),
        buffer.iter_at_mark(&buffer.get_insert()).offset(),
        buffer.iter_at_mark(&buffer.selection_bound()).offset(),
    )
}

/// The same keys in Lemma's editor and in a bare GtkSourceView view give the same text, cursor
/// and selection after `:s`, the same place for Ctrl+O (the jump list), the same match for `n`
/// (the search pattern), and the same text after `u`: on the cases measured against
/// GtkSourceView 5.22 (`vim::substitute`'s unit tests) and on random ones.
fn colon_s_matches_gtksourceview() {
    let f = fixture("substitute", "\n");
    let bare = Bare::new();
    let lines = "l1 a\nl2 a\nl3 a\nl4 a\n";
    let mut cases: Vec<(String, i32, String)> = [
        ("aaa\n", 0, ":s/a/b/g\n"),
        ("aaaaaa\n", 0, ":s/aa/X/g\n"),
        ("a a a\n", 2, ":s/a/b/g\n"),
        ("a a a\n", 2, ":s/a/b/\n"),
        ("x a\nb a\nc a\nd a\n", 0, ":1,3s/a/Z/\n"),
        ("x a\nb a\nc a\nd a\n", 0, ":%s/a/Z/\n"),
        ("x a a\nb a a\n", 0, ":%s/a/Z/\n"),
        ("x a a\nb a a\n", 0, ":%s/a/Z/g\n"),
        ("foo bar\n", 0, ":s/(foo) (bar)/\\2 \\1/\n"),
        ("foo bar\n", 0, ":s/\\(foo\\) \\(bar\\)/\\2 \\1/\n"),
        ("foo bar\n", 0, ":s/(?<x>foo)/\\g<x>\\g<0>/\n"),
        ("foo bar\n", 0, ":s/foo/a\\nb/\n"),
        ("Foo foo\n", 0, ":s/foo/X/gi\n"),
        ("Foo foo\n", 0, ":s/foo/X/g\n"),
        ("ab\ncd\nef\n", 0, ":%s/b\\ncd\\ne/X/\n"),
        ("caf\u{e9} x\n", 0, ":s/\\w+/W/g\n"),
        ("a/b\n", 0, ":s/\\//-/\n"),
        ("a#b#a\n", 0, ":s#a#\\##g\n"),
        ("  x a\n y\n", 0, ":%s/a/Q/\n"),
        ("\tfoo\n\tbar foo\n", 0, ":%s/foo/F/\n"),
        ("one two\n  three two\nfour\n", 0, ":%s/two/2/\n"),
        ("aXa aXa\n", 0, ":s/a/aa/g\n"),
        ("abc abc\n", 0, ":s/(b)(c)/\\u\\1\\U\\2x\\E!/g\n"),
        ("abc\n", 0, ":s/b/\\U&x/\n"),
        ("a\na\na\n", 0, ":%s/a/b\\nc/\n"),
        ("a a\na\n", 0, ":s/a/\\n/g\n"),
        ("x foo\ny\n", 6, ":s/foo/F/\n"),
        (lines, 6, ":.,$s/a/B/\n"),
        (lines, 0, ":.,+1s/a/B/\n"),
        (lines, 0, ":2,$s/a/B/g\n"),
        (lines, 0, ":3,1s/a/B/\n"),
        (lines, 0, ":5s/a/B/\n"),
        (lines, 0, ":.s/a/B/\n"),
        (lines, 5, "Vj:s/a/S/\n"),
        (lines, 7, "vj:s/a/S/\n"),
        (lines, 12, "vk:s/l/L/g\n"),
        ("x\n", 0, ":s/[/y/\n"),
        ("x\n", 0, ":s/x/\\g/\n"),
        ("x\ny\n", 0, ":%s/^/> /\n"),
        ("x\ny\n", 0, ":%s/$/;/\n"),
        ("x\n", 0, ":s/x\n"),
        // CRLF line breaks: taking the `\r` of a `\r\n` moves no line, an inserted `\r` ends one
        // even before a `\n` (as GTK splits an insertion into lines). (Taking the `\n` joins two
        // lines with a lone `\r` inside, where GTK's block cursor logs criticals; see the unit
        // tests.)
        ("a\r\nb\r\nc\r\n", 0, ":%s/\\r//\n"),
        ("a\r\nb\r\nc\r\n", 0, ":s/\\r//g\n"),
        ("x\nx\nx\n", 0, ":%s/x/a\\r/\n"),
        ("x a\u{2029}y a\n", 0, ":%s/a/Z/\n"),
    ]
    .into_iter()
    .map(|(text, cursor, keys)| (text.to_owned(), cursor, keys.to_owned()))
    .collect();
    let seed: u64 = std::env::var("LEMMA_TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_5b5b);
    let ops: usize = std::env::var("LEMMA_TEST_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    println!("    seed {seed}, {ops} random cases");
    let mut state = seed;
    let mut next = |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    // GtkSourceView searches the text as its replacements changed it, Lemma the text before
    // them (see `vim::substitute`): the random replacements never make a match with the text
    // around them (no pattern matches `X`, `Y`, `Z`, `<` or `>`, and the matched text stays
    // between `<` and `>`), and lookbehinds look back one character. (A reference to a group
    // the pattern lacks can hang GtkSourceView's command: `:2,4s/x|\s/<\1>/` on
    // `"ab\nab\nb     x aax  b  axaaa aa\nba"` never returns.)
    let pieces = ["a", "aa", "b", "ab", " ", "  ", "\n", "\n", "x"];
    let patterns = [
        "a", "aa", "a+", "ab?", "a b", "b\\n", "^a", "a$", "\\ba", "(a)(b)?", "\\s+", "[ab]{2}",
        "a|b", "(?<=a)b", "\\n\\n", "a\\nb|b", "A", "x|\\s",
    ];
    let replacements = ["X", "Y\\nZ", "<\\0>", "\\n"];
    let ranges = ["", "", "%", "%", "1,3", "2,4", ".,$", ".,+1", "'<,'>"];
    let flags = ["", "g", "gi", "i"];
    for _ in 0..ops {
        let text: String = (0..next(40) + 1)
            .map(|_| pieces[next(pieces.len())])
            .collect();
        let cursor = next(text.chars().count() + 1) as i32;
        // `'<,'>` comes from a selection: Vim puts it in the command bar.
        let (select, range) = match ranges[next(ranges.len())] {
            "'<,'>" => (["v", "vj", "Vk", "vl"][next(4)], ""),
            r => ("", r),
        };
        let keys = format!(
            "{select}:{range}s/{}/{}/{}\n",
            patterns[next(patterns.len())],
            replacements[next(replacements.len())],
            flags[next(flags.len())]
        );
        cases.push((text, cursor, keys));
    }
    let mut failures = Vec::new();
    let mut broken = 0;
    'cases: for (text, cursor, keys) in &cases {
        let mut seen = Vec::new();
        for (name, keys) in [
            ("substitute", keys.as_str()),
            ("Ctrl+O", "\x0f"),
            ("n", "n"),
            ("u", "u"),
        ] {
            if name == "substitute" {
                f.type_keys("\x1b");
                bare.type_keys("\x1b");
                reset(f.editor.buffer(), text, *cursor);
                reset(&bare.view.buffer(), text, *cursor);
            }
            f.type_keys(keys);
            let logged = common::logged_problems();
            bare.type_keys(keys);
            let problems = common::logged_problems() - logged;
            if problems > 0 {
                // GtkSourceView's replace fails (`regex_replace: assertion 'g_str_has_suffix
                // (subject_replaced, suffix)' failed`) when a match that could go on (`ab?`)
                // is followed right away by another one, and its command stops there. Lemma
                // does what it would have done (see `vim::substitute`).
                common::expect_logged_problems(problems);
                broken += 1;
                continue 'cases;
            }
            let (lemma, gtksourceview) = (look(f.editor.buffer()), look(&bare.view.buffer()));
            if lemma != gtksourceview {
                failures.push(format!(
                    "{text:?} at {cursor}, {keys:?} then {seen:?} {name}: Lemma {lemma:?}, \
                     GtkSourceView {gtksourceview:?}"
                ));
                break;
            }
            seen.push(name);
        }
        if seen.len() == 4 && f.text() != *text {
            failures.push(format!("{text:?}, {keys:?}: u gave {:?}", f.text()));
        }
    }
    println!(
        "    {} cases; in {broken} of them GtkSourceView's own replace failed",
        cases.len()
    );
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A substitution is one edit through the document (`Origin::User`) and one undo step, and the
/// pattern becomes the one `n` looks for.
fn colon_s_is_one_undo_step() {
    let text: String = (1..=40)
        .map(|i| format!("line {i} with a word\n"))
        .collect();
    let f = fixture("substitute-undo", &text);
    let changes = Rc::new(Cell::new(0));
    let counted = changes.clone();
    let id = f
        .editor
        .document()
        .connect_before_apply(move |_, edits, origin| {
            assert_eq!(origin, lemma_core::Origin::User);
            counted.set(counted.get() + edits.len());
        });
    f.type_keys(":%s/with/WITH/g\n");
    assert_eq!(changes.get(), 40, "one apply_edits of every match");
    assert_eq!(f.text(), text.replace("with", "WITH"));
    // On the first non-blank of the last line changed.
    let buffer = f.editor.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    assert_eq!((cursor.line(), cursor.line_offset()), (39, 0));
    f.type_keys("u");
    assert_eq!(f.text(), text, "one undo step");
    f.editor.document().disconnect(id);
    // `n` looks for the substitution's pattern from the cursor.
    f.type_keys("ggn");
    let cursor = buffer.iter_at_mark(&buffer.get_insert());
    assert_eq!((cursor.line(), cursor.line_offset()), (0, 7));
}

/// A document of the first `lines` lines of `large.tex` with Vim on, and the cursor on `line`.
fn substitute_fixture(name: &str, lines: usize, line: i32) -> Fixture {
    let text: String = std::fs::read_to_string(common::fixtures().join("large.tex"))
        .unwrap()
        .split_inclusive('\n')
        .take(lines)
        .collect();
    let f = fixture(name, &text);
    let buffer = f.editor.buffer();
    buffer.place_cursor(&buffer.iter_at_line(line).unwrap());
    f
}

/// `command` typed up to its Return; then the Return key's own time (the substitution) and the
/// undo's, `rounds` times.
fn time_substitute(f: &Fixture, command: &str, rounds: usize) -> (Vec<Duration>, Vec<Duration>) {
    let im = f.im();
    let view = f.editor.view();
    let original = f.text();
    let buffer = f.editor.buffer();
    let cursor = buffer.iter_at_mark(&buffer.get_insert()).offset();
    let (mut runs, mut undos) = (Vec::new(), Vec::new());
    for _ in 0..rounds {
        buffer.place_cursor(&buffer.iter_at_offset(cursor));
        f.type_keys(command);
        let t = Instant::now();
        vim::feed_key(&im, view, gdk::Key::Return, gdk::ModifierType::empty());
        runs.push(t.elapsed());
        assert_ne!(f.text(), original, "{command} replaced nothing");
        common::pump_for(Duration::from_millis(20));
        let t = Instant::now();
        vim::feed_key(&im, view, gdk::Key::u, gdk::ModifierType::empty());
        undos.push(t.elapsed());
        assert_eq!(f.text(), original, "undo");
        common::pump_for(Duration::from_millis(20));
    }
    (runs, undos)
}

/// The median and the maximum.
fn median(mut v: Vec<Duration>) -> (Duration, Duration) {
    v.sort();
    (v[v.len() / 2], *v.last().unwrap())
}

/// `:N,Ms` over 1000 lines of a 10 000-line document (336 matches): about 35 ms, where
/// GtkSourceView took 4–8 s at a load of 5–9, each replacement scanning the buffer again.
fn perf_substitute_range() {
    let f = substitute_fixture("perf-substitute-range", 10_000, 4999);
    let (runs, undos) = time_substitute(&f, ":5000,6000s/with/WITH/g", 5);
    let (run, run_max) = median(runs);
    let (undo, undo_max) = median(undos);
    println!(
        "    :5000,6000s/with/WITH/g on 10 000 lines: median {run:?} (max {run_max:?}); undo \
         {undo:?} (max {undo_max:?})"
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(run <= Duration::from_millis(100), "median {run:?}");
    }
}

/// `:%s` of 3328 matches in 10 000 lines: about 0.27 s (18–50 s in GtkSourceView).
/// `LEMMA_PERF_ROUNDS` (5) sets the number of runs.
fn perf_substitute_whole() {
    let rounds = std::env::var("LEMMA_PERF_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let f = substitute_fixture("perf-substitute-whole", 10_000, 0);
    let (runs, undos) = time_substitute(&f, ":%s/with/WITH/g", rounds);
    let (run, run_max) = median(runs);
    let (undo, undo_max) = median(undos);
    println!(
        "    :%s/with/WITH/g on 10 000 lines: median {run:?} (max {run_max:?}); undo {undo:?} \
         (max {undo_max:?})"
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(run <= Duration::from_millis(600), "median {run:?}");
    }
}

/// `:s` on one of the last lines of `large.tex` (60 000 lines), which still searches the
/// buffer's matches from its top: the 20 000 `with` above it (about 20 ms; 0.27–0.55 s in
/// GtkSourceView), and (printed) the 112 000 `a` (about 25 ms; 1.3–2.4 s).
fn perf_substitute_line() {
    let f = substitute_fixture("perf-substitute-line", usize::MAX, 59_994);
    let (lines, _) = time_substitute(&f, ":s/with/WITH/g", 5);
    let (line, line_max) = median(lines);
    let (worst, _) = time_substitute(&f, ":s/a/A/g", 5);
    let (worst, worst_max) = median(worst);
    println!(
        "    large.tex line 59 995: :s/with/WITH/g median {line:?} (max {line_max:?}); \
         :s/a/A/g median {worst:?} (max {worst_max:?})"
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(line <= Duration::from_millis(60), "median {line:?}");
    }
}

/// `:%s` of 20 000 matches in `large.tex` (60 000 lines): about 1.5 s (more than ten minutes in
/// GtkSourceView). Most of it is the 40 000 buffer changes: GTK's own, those of Vim's search
/// context (GtkSourceView updates it after every change once Vim has searched) and Lemma's
/// observers.
fn perf_substitute_large() {
    let f = substitute_fixture("perf-substitute-large", usize::MAX, 0);
    let (runs, undos) = time_substitute(&f, ":%s/with/WITH/g", 3);
    let (run, run_max) = median(runs);
    let (undo, undo_max) = median(undos);
    println!(
        "    large.tex: :%s/with/WITH/g median {run:?} (max {run_max:?}), undo {undo:?} (max \
         {undo_max:?})"
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(run <= Duration::from_secs(4), "median {run:?}");
    }
}
