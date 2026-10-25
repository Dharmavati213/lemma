//! Vim mode on a real window (WP0.2 acceptance 5): keys are typed into GtkSourceView's
//! `VimIMContext` with `gtk_im_context_filter_key`, which builds the same key events a keyboard
//! produces (keycodes come from the display's keymap). That is exactly what the capture-phase key
//! controller does with a real key press, so the command bar (`:w⏎`) behaves as it does
//! interactively. A key Vim does not consume would continue to the `GtkTextView`; GTK has no API
//! to inject events, so [`Fixture::type_keys`] emulates that step for printable characters.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_vim`.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::editor::{Editor, VimMode};
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

/// Set in a child process: the joins that must never hang run there (see
/// [`joins_near_the_end_return`]).
const JOIN_CHILD_ENV: &str = "LEMMA_GTK_VIM_JOIN_CHILD";

fn main() -> ExitCode {
    if std::env::var_os(JOIN_CHILD_ENV).is_some() {
        return common::run(&[("join_child", join_child)]);
    }
    common::run(&[
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
    ])
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
    let display = surface.display();
    let keyboard = display
        .default_seat()
        .and_then(|s| s.keyboard())
        .expect("a keyboard device");
    for c in keys.chars() {
        let keyval = match c {
            '\n' => gdk::Key::Return,
            '\x1b' => gdk::Key::Escape,
            // `gdk::Key` has no safe constructor from a keyval; it is a `u32` GValue.
            c => gdk::unicode_to_keyval(u32::from(c))
                .to_value()
                .get::<gdk::Key>()
                .expect("a keyval"),
        };
        let key = display
            .map_keyval(keyval)
            .and_then(|keys| keys.into_iter().min_by_key(|k| k.level()))
            .unwrap_or_else(|| panic!("no keycode for {c:?}"));
        let state = if key.level() > 0 {
            gdk::ModifierType::SHIFT_MASK
        } else {
            gdk::ModifierType::empty()
        };
        if guarded && editor.vim_blocks_key(keyval, state) {
            common::pump_for(Duration::from_millis(2));
            continue;
        }
        let consumed = im.filter_key(
            true,
            &surface,
            &keyboard,
            0,
            key.keycode(),
            state,
            key.group(),
        );
        im.filter_key(
            false,
            &surface,
            &keyboard,
            0,
            key.keycode(),
            state,
            key.group(),
        );
        if !consumed && c != '\x1b' {
            // What the text view does with a key Vim lets through (insert mode).
            editor
                .buffer()
                .insert_interactive_at_cursor(&c.to_string(), true);
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
    // Built-in commands are offered first too, and run when not handled.
    f.type_keys(":s/a/b/\n");
    assert_eq!(*seen.borrow(), [":Frobnicate", ":s/a/b/"]);
    assert_eq!(f.text(), "b\n");
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
