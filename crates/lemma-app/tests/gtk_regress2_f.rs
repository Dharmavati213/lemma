//! Regression tests for verified area-F defects of the second review round (the M2 shell:
//! documents, live settings, app and window hooks). Each test reproduces one defect and fails
//! until it is fixed.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_regress2_f`.
//!
//! The `[keys]` defect aborts the process; its tests run the scenario in a child process (this
//! binary again, with `LEMMA_REGRESS2_F_CHILD=<probe>`) and fail when the child dies. The probe
//! `leak_workaround` (run by hand) shows that the closed-tab leak goes away when the gutter
//! renderers are removed, and one frame drawn, before the tab closes.

mod common;

use adw::prelude::*;
use lemma_agent::EditorRequest;
use lemma_app::agent_edits;
use lemma_app::editor::Editor;
use lemma_app::review_ui::{self, Decision};
use lemma_app::settings::{self, ConfigFile, SettingsFiles};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::{Config, KeysCfg};
use lemma_core::ProjectRoot;
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

const CHILD_ENV: &str = "LEMMA_REGRESS2_F_CHILD";

fn main() -> ExitCode {
    // Nothing of these tests (or of the `lemma` runs they start) may reach the user's settings,
    // state or data directories.
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-regress2-f-xdg-{}", std::process::id()));
    for (var, sub) in [
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let dir = home.join(sub);
        std::fs::create_dir_all(&dir).expect("create a test XDG directory");
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var(var, &dir) };
    }
    // SAFETY: as above.
    unsafe { std::env::set_var("LEMMA_CONFIG", "/dev/null") };
    if let Ok(probe) = std::env::var(CHILD_ENV) {
        return run_child(&probe);
    }
    common::run(&[
        // F-2
        (
            "regress2_older_own_write_is_reloaded",
            regress2_older_own_write_is_reloaded,
        ),
        (
            "regress2_closed_doc_older_own_write_reads_disk",
            regress2_closed_doc_older_own_write_reads_disk,
        ),
        // F-3 / F-9
        (
            "regress2_symlinked_config_reloads",
            regress2_symlinked_config_reloads,
        ),
        // F-4 / F-10 / F-17
        (
            "regress2_closed_tab_frees_its_buffer",
            regress2_closed_tab_frees_its_buffer,
        ),
        // F-5 / F-13
        (
            "regress2_pdf_dark_invert_follows_reload",
            regress2_pdf_dark_invert_follows_reload,
        ),
        // F-6
        (
            "regress2_keys_bad_action_name_at_startup",
            regress2_keys_bad_action_name_at_startup,
        ),
        (
            "regress2_keys_bad_action_name_on_reload",
            regress2_keys_bad_action_name_on_reload,
        ),
        // F-7
        (
            "regress2_stale_overwrite_after_discard",
            regress2_stale_overwrite_after_discard,
        ),
        // F-8
        (
            "regress2_overwrite_toasts_do_not_pile_up",
            regress2_overwrite_toasts_do_not_pile_up,
        ),
        // F-11
        (
            "regress2_tab_tooltip_path_with_ampersand",
            regress2_tab_tooltip_path_with_ampersand,
        ),
        // F-14
        (
            "regress2_directory_argument_is_the_root",
            regress2_directory_argument_is_the_root,
        ),
        // Known item: memory (ARCHITECTURE §7)
        (
            "regress2_inactive_window_gives_memory_back",
            regress2_inactive_window_gives_memory_back,
        ),
        (
            "regress2_closed_large_tab_gives_memory_back",
            regress2_closed_large_tab_gives_memory_back,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

/// Runs the main context until `done` or `timeout`; returns whether `done` became true.
fn pump_until_or(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

// ----- fixture -----

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
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        block_on(self.state.open_file(&self.dir.join(file))).expect("open")
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    fn disk(&self, file: &str) -> String {
        std::fs::read_to_string(self.path(file)).unwrap()
    }

    /// Writes `text` to `file` as another program would (`git checkout`), dated `secs` in the
    /// future (modification times are compared in whole seconds).
    fn external_write(&self, file: &str, text: &str, secs: u64) {
        std::fs::write(self.path(file), text).unwrap();
        std::fs::File::options()
            .write(true)
            .open(self.path(file))
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(secs))
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut child = widget.first_child();
    while let Some(c) = child {
        out.extend(descendants(&c));
        child = c.next_sibling();
        out.push(c);
    }
    out
}

/// The Overwrite buttons of the toast on screen.
fn overwrite_buttons(state: &WindowState) -> Vec<gtk::Button> {
    descendants(&state.toasts)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .filter(|b| b.label().is_some_and(|l| l.contains("Overwrite")))
        .collect()
}

/// Dismisses the toast on screen with its close button; false when no toast is shown.
fn dismiss_shown_toast(state: &WindowState) -> bool {
    let close = descendants(&state.toasts)
        .into_iter()
        .filter(|w| w.type_().name() == "AdwToastWidget")
        .flat_map(|toast| descendants(&toast))
        .filter_map(|w| w.downcast::<gtk::Button>().ok())
        .find(|b| b.icon_name().is_some_and(|i| i == "window-close-symbolic"));
    match close {
        Some(button) => {
            button.emit_clicked();
            true
        }
        None => false,
    }
}

// ----- F-2: an older own write is not "ours" any more -----

/// Branch switch: Lemma saves "A base"; `git checkout b` writes "B base" (the editor reloads);
/// `git checkout a` writes "A base" again. That is a change from what the editor holds, so it must
/// reload again; the hash of the earlier own write made the file monitor ignore it.
fn regress2_older_own_write_is_reloaded() {
    let f = fixture("own-write-branch", &[("main.tex", "base\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "A ");
    block_on(doc.save()).expect("save A");
    assert_eq!(f.disk("main.tex"), "A base\n");
    common::pump_for(Duration::from_millis(500));

    f.external_write("main.tex", "B base\n", 5);
    common::pump_until(TIMEOUT, || doc.text() == "B base\n");
    assert!(!doc.is_dirty());
    common::pump_for(Duration::from_millis(300));

    f.external_write("main.tex", "A base\n", 10);
    let followed = pump_until_or(Duration::from_secs(4), || doc.text() == "A base\n");
    assert!(
        followed,
        "the disk holds {:?} but the clean editor still shows {:?} (no reload, no bar): the \
         file's hash is that of an earlier own write",
        f.disk("main.tex"),
        doc.text()
    );
}

/// A closed document whose file was changed back to an earlier own write is stale, and opening
/// it must show the file: agent edit c1 → C1 (written), Reject (c1 written), another program
/// writes C1 again.
fn regress2_closed_doc_older_own_write_reads_disk() {
    let f = fixture(
        "own-write-closed",
        &[("main.tex", "x\n"), ("ch.tex", "c1\nc2\n")],
    );
    let reply = block_on(agent_edits::apply(
        &f.state.workspace,
        1,
        EditorRequest::Edit {
            path: "ch.tex".into(),
            old_string: "c1\n".into(),
            new_string: "C1\n".into(),
            replace_all: false,
        },
    ));
    assert!(!reply.is_error, "{reply:?}");
    let doc = f.state.workspace.get(&f.path("ch.tex")).unwrap();
    assert!(!doc.is_open());
    assert_eq!(f.disk("ch.tex"), "C1\nc2\n");
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || f.disk("ch.tex") == "c1\nc2\n");
    common::pump_for(Duration::from_millis(500));

    f.external_write("ch.tex", "C1\nc2\n", 5);
    common::pump_for(Duration::from_millis(1500));
    let editor = f.open("ch.tex");
    assert_eq!(
        editor.document().text(),
        f.disk("ch.tex"),
        "the opened document shows stale text instead of the file on disk"
    );
}

// ----- F-3 / F-9: symlinked settings files -----

/// `~/.config/lemma/config.toml` is a symlink into a dotfiles directory (GNU stow). Vim and
/// Neovim write a symlinked file in place (backupcopy), which changes the target in the dotfiles
/// directory: the reload must still happen.
fn regress2_symlinked_config_reloads() {
    let dir = common::temp_dir("settings-symlink");
    let (conf, dots) = (dir.join("conf"), dir.join("dotfiles"));
    std::fs::create_dir_all(&conf).unwrap();
    std::fs::create_dir_all(&dots).unwrap();
    std::fs::write(dots.join("config.toml"), "[editor]\nwrap = true\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dots.join("config.toml"), conf.join("config.toml")).unwrap();
    let files = SettingsFiles::next_to(conf.join("config.toml"));
    let seen: Rc<RefCell<Vec<ConfigFile>>> = Rc::default();
    let sink = seen.clone();
    let watcher = settings::Watcher::start(&files, move |file| sink.borrow_mut().push(file));
    assert_eq!(watcher.len(), 3);
    common::pump_for(Duration::from_millis(200));

    // Control: the link's own directory is watched (a new snippets.toml next to the link).
    std::fs::write(&files.snippets, "").unwrap();
    common::pump_until(TIMEOUT, || seen.borrow().contains(&ConfigFile::Snippets));

    // An in-place write through the link, as Vim does for a symlink.
    let mut file = std::fs::File::options()
        .write(true)
        .truncate(true)
        .open(&files.config)
        .unwrap();
    file.write_all(b"[editor]\nwrap = false\n").unwrap();
    drop(file);
    let reloaded = pump_until_or(Duration::from_secs(3), || {
        seen.borrow().contains(&ConfigFile::Config)
    });
    drop(watcher);
    assert!(
        reloaded,
        "config.toml (a symlink) was written through the link but no reload was scheduled: {:?}",
        seen.borrow()
    );
}

// ----- F-4 / F-10 / F-17: a closed tab keeps its view and buffer -----

/// After its tab closes (and every Rust handle is gone), the editor's view and its buffer, with
/// the whole text, tags and undo history, must be finalized.
fn regress2_closed_tab_frees_its_buffer() {
    let f = fixture(
        "closed-tab-leak",
        &[
            ("main.tex", "main\n"),
            ("ch.tex", &"some text\n".repeat(200)),
        ],
    );
    let _main = f.open("main.tex");
    let editor = f.open("ch.tex");
    let drawn = Rc::new(Cell::new(false));
    let flag = drawn.clone();
    editor.after_first_frame(move |_| flag.set(true));
    common::pump_until(TIMEOUT, || drawn.get());
    common::pump_for(Duration::from_millis(500));

    let editor_weak = Rc::downgrade(&editor);
    let doc_weak = Rc::downgrade(editor.document());
    let view_weak = editor.view().downgrade();
    let buffer_weak = editor.buffer().downgrade();
    f.state.close_editor(&editor, true);
    drop(editor);
    common::pump_for(Duration::from_millis(1500));

    assert!(editor_weak.upgrade().is_none(), "the Editor is kept");
    assert!(doc_weak.upgrade().is_none(), "the Document is kept");
    let (view, buffer) = (view_weak.upgrade(), buffer_weak.upgrade());
    let report = format!(
        "view alive: {} (refs {:?}), buffer alive: {} (refs {:?})",
        view.is_some(),
        view.as_ref().map(|v| v.ref_count()),
        buffer.is_some(),
        buffer.as_ref().map(|b| b.ref_count())
    );
    drop((view, buffer));
    assert!(
        buffer_weak.upgrade().is_none() && view_weak.upgrade().is_none(),
        "the closed tab's view and buffer were never finalized: {report}"
    );

    // Ctrl+W (`win.close-tab`) with no other handle on the editor: it goes while its tab is
    // being detached.
    let editor = f.open("ch.tex");
    let drawn = Rc::new(Cell::new(false));
    let flag = drawn.clone();
    editor.after_first_frame(move |_| flag.set(true));
    common::pump_until(TIMEOUT, || drawn.get());
    let (view_weak, buffer_weak) = (editor.view().downgrade(), editor.buffer().downgrade());
    drop(editor);
    WidgetExt::activate_action(&f.state.window, "win.close-tab", None).unwrap();
    common::pump_for(Duration::from_millis(500));
    assert_eq!(f.state.editors().len(), 1, "the tab closed");
    assert!(
        buffer_weak.upgrade().is_none() && view_weak.upgrade().is_none(),
        "the view and buffer of a tab closed with Ctrl+W were never finalized"
    );
}

// ----- F-5 / F-13: [pdf] keys and live reload -----

/// Puts the system style back when the test ends, even when it fails.
struct StyleGuard;

impl Drop for StyleGuard {
    fn drop(&mut self) {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
        common::pump_for(Duration::from_millis(50));
    }
}

/// With a dark system style, the PDF is inverted (`pdf.dark_invert`, on by default). A reload of
/// config.toml that turns `dark_invert` off must show the pages normally again.
fn regress2_pdf_dark_invert_follows_reload() {
    let _guard = StyleGuard;
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let f = fixture("pdf-dark-reload", &[("main.tex", "x\n")]);
    common::pump_until(TIMEOUT, || adw::StyleManager::default().is_dark());
    let pdf = f.state.pdf.get().expect("the PDF controller");
    common::pump_until(TIMEOUT, || pdf.is_dark());

    let mut config = (*f.state.config.get()).clone();
    config.pdf.dark_invert = false;
    f.state.set_config(Rc::new(config));
    common::pump_for(Duration::from_millis(200));
    assert!(
        !pdf.is_dark(),
        "config.toml now says dark_invert = false, but the PDF stays inverted"
    );
}

// ----- F-6: a malformed [keys] action name aborts -----

/// Runs this binary with `probe`; returns its exit code (`None`: killed by a signal).
fn run_probe(probe: &str) -> Option<i32> {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .env(CHILD_ENV, probe)
        .spawn()
        .expect("spawn the child");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("wait for the child") {
            println!("  child {probe}: {status}");
            return status.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child {probe} still running after {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `[keys] "win.toggle pdf" = "F7"` (a typo: a space) in config.toml at start-up.
fn regress2_keys_bad_action_name_at_startup() {
    let code = run_probe("keys_startup");
    assert_eq!(
        code,
        Some(0),
        "an invalid [keys] action name at start-up killed the process (GTK aborts in \
         gtk_normalise_detailed_action_name); it must be reported and skipped"
    );
}

/// The same typo (`"win.build:"`) saved into config.toml while Lemma runs: the live reload
/// must report it, not abort with the user's unsaved work.
fn regress2_keys_bad_action_name_on_reload() {
    let code = run_probe("keys_reload");
    assert_eq!(
        code,
        Some(0),
        "an invalid [keys] action name in a live reload killed the process"
    );
}

fn run_child(probe: &str) -> ExitCode {
    if gtk::init().is_err() {
        println!("SKIP: no display");
        return ExitCode::SUCCESS;
    }
    sourceview5::init();
    match probe {
        "keys_startup" => {
            let mut keys = KeysCfg::default();
            keys.bindings
                .insert("win.toggle pdf".to_owned(), "F7".to_owned());
            lemma_app::app::set_accels(&common::application(), &keys);
            println!("  survived set_accels");
        }
        "keys_reload" => {
            let f = fixture("keys-reload", &[("main.tex", "x\n")]);
            let _editor = f.open("main.tex");
            let mut config = (*f.state.config.get()).clone();
            config
                .keys
                .bindings
                .insert("win.build:".to_owned(), "<Control>b".to_owned());
            f.state.set_config(Rc::new(config));
            println!("  survived set_config");
        }
        // Manual probe (not a test): the closed-tab test with the gutter renderers removed (and
        // one frame drawn) before the tab closes.
        "leak_workaround" => {
            let f = fixture(
                "closed-tab-workaround",
                &[
                    ("main.tex", "main\n"),
                    ("ch.tex", &"some text\n".repeat(200)),
                ],
            );
            let _main = f.open("main.tex");
            let editor = f.open("ch.tex");
            let drawn = Rc::new(Cell::new(false));
            let flag = drawn.clone();
            editor.after_first_frame(move |_| flag.set(true));
            common::pump_until(TIMEOUT, || drawn.get());
            common::pump_for(Duration::from_millis(500));
            let gutter =
                sourceview5::prelude::ViewExt::gutter(editor.view(), gtk::TextWindowType::Left);
            let mut child = gutter.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                if let Ok(renderer) = c.downcast::<sourceview5::GutterRenderer>() {
                    gutter.remove(&renderer);
                }
            }
            editor.view().queue_draw();
            common::pump_for(Duration::from_millis(1500));
            let (view_weak, buffer_weak) = (editor.view().downgrade(), editor.buffer().downgrade());
            f.state.close_editor(&editor, true);
            drop(editor);
            common::pump_for(Duration::from_millis(1500));
            println!(
                "  renderers removed first: view alive {}, buffer alive {}",
                view_weak.upgrade().is_some(),
                buffer_weak.upgrade().is_some()
            );
        }
        other => panic!("unknown probe {other}"),
    }
    ExitCode::SUCCESS
}

// ----- F-7: a stale Overwrite toast -----

/// Ctrl+S is refused (the file changed on disk) and the Overwrite toast appears; the user then
/// closes the tab and discards the changes, and the file changes again. The toast stays, and its
/// button must not write the discarded text over the newest file.
fn regress2_stale_overwrite_after_discard() {
    let f = fixture(
        "stale-overwrite",
        &[("main.tex", "original\n"), ("other.tex", "o\n")],
    );
    let _other = f.open("other.tex");
    let editor = f.open("main.tex");
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "UNWANTED EDIT\n");
    f.external_write("main.tex", "EXTERNAL v1\n", 5);
    f.state.save_document(editor.document());
    common::pump_until(TIMEOUT, || !overwrite_buttons(&f.state).is_empty());

    f.state.close_editor(&editor, true);
    drop(editor);
    common::pump_for(Duration::from_millis(300));
    assert!(f.state.workspace.get(&f.path("main.tex")).is_none());
    f.external_write("main.tex", "EXTERNAL v2 (newest)\n", 10);
    common::pump_for(Duration::from_millis(300));

    let stale = overwrite_buttons(&f.state);
    println!(
        "  Overwrite toasts still shown after the discard: {}",
        stale.len()
    );
    if let Some(button) = stale.first() {
        button.emit_clicked();
    }
    common::pump_for(Duration::from_millis(1000));
    assert_eq!(
        f.disk("main.tex"),
        "EXTERNAL v2 (newest)\n",
        "the stale Overwrite wrote the discarded text over the newest file"
    );
}

// ----- F-8: Overwrite toasts pile up -----

/// Three refused saves of the same document must leave one Overwrite toast, not three queued
/// ones that each hold back every later toast until dismissed.
fn regress2_overwrite_toasts_do_not_pile_up() {
    let f = fixture("overwrite-pile", &[("main.tex", "original\n")]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "typed\n");
    f.external_write("main.tex", "EXTERNAL\n", 5);
    for _ in 0..3 {
        // Ctrl+S, refused each time.
        f.state.save_document(&doc);
        common::pump_until(TIMEOUT, || !overwrite_buttons(&f.state).is_empty());
        common::pump_for(Duration::from_millis(500));
    }
    assert_eq!(f.disk("main.tex"), "EXTERNAL\n");
    let mut shown = 0;
    while !overwrite_buttons(&f.state).is_empty() && shown < 10 {
        shown += 1;
        assert!(dismiss_shown_toast(&f.state), "a toast with a close button");
        common::pump_for(Duration::from_millis(400));
    }
    assert_eq!(f.state.editors().len(), 1, "the tab is still open");
    assert_eq!(
        shown, 1,
        "the Overwrite toasts for main.tex were shown one after another"
    );
}

// ----- F-11: tab tooltips are markup -----

/// A project in "Smith & Jones": the tab's tooltip (Pango markup in AdwTabPage) must show the
/// path.
fn regress2_tab_tooltip_path_with_ampersand() {
    let f = fixture("Smith & Jones", &[("main.tex", "x\n"), ("ch.tex", "y\n")]);
    let _main = f.open("main.tex");
    let _ch = f.open("ch.tex");
    common::pump_for(Duration::from_millis(300));
    let path = f.path("ch.tex").display().to_string();
    let tabs: Vec<gtk::Widget> = descendants(&f.state.tab_bar)
        .into_iter()
        .filter(|w| w.type_().name() == "AdwTab")
        .collect();
    assert!(!tabs.is_empty(), "the tab bar has tabs");
    let tooltips: Vec<(Option<glib::GString>, Option<glib::GString>)> = tabs
        .iter()
        .map(|t| (t.tooltip_markup(), t.tooltip_text()))
        .collect();
    println!("  (markup, text) of each tab: {tooltips:?}");
    assert!(
        tooltips
            .iter()
            .any(|(_, text)| text.as_deref() == Some(path.as_str())),
        "no tab shows the tooltip {path:?}: the unescaped '&' makes the markup invalid"
    );
}

// ----- F-14: a directory as FILE -----

/// `lemma DIR` (`lemma .`, as with `nvim .`) must make DIR the project root, not fail to open it
/// with DIR's parent as the root.
fn regress2_directory_argument_is_the_root() {
    let dir = common::temp_dir("dir-argument");
    let project = dir.join("thesis");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.tex"), "x\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lemma"))
        .arg(&project)
        .args(["--quit-after-ms", "1500"])
        .current_dir(&dir)
        .output()
        .expect("run lemma");
    let stderr = String::from_utf8_lossy(&out.stderr);
    print!("{stderr}");
    assert!(
        out.status.success() && !stderr.contains("Is a directory"),
        "`lemma {}` failed ({}): {stderr}",
        project.display(),
        out.status
    );
}

// ----- known item: memory -----

/// A window in the background gives memory back after a while (ARCHITECTURE §7): the PDF cache
/// keeps only the pages on screen. `trim_memory` does it at once.
fn regress2_inactive_window_gives_memory_back() {
    let f = fixture("trim-inactive", &[("main.tex", "x\n")]);
    f.state.pdf_pane.set_visible(true);
    let _editor = f.open("main.tex");
    let pdf = f.state.pdf.get().expect("the PDF controller");
    pdf.open(&PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/pdf/stress.pdf"
    )));
    let view = pdf.view().clone();
    let drawn = |view: &lemma_app::pdf::pdf_view::PdfView| {
        let visible = view.visible_pages();
        view.page_count() > 0
            && view.render_scale_q() > 0
            && !visible.is_empty()
            && visible.clone().all(|p| view.is_page_rendered(p))
    };
    common::pump_until(TIMEOUT, || drawn(&view));
    // Scroll through every page so the cache holds many.
    let content = view.layout().unwrap().content_size(view.zoom()).1;
    let step = f64::from(view.height()) * 0.8;
    let mut y = 0.0;
    while y < content {
        view.set_scroll_position(0.0, y);
        common::pump_until(TIMEOUT, || drawn(&view));
        y += step;
    }
    view.set_scroll_position(0.0, 0.0);
    common::pump_until(TIMEOUT, || drawn(&view));
    let full = view.cache_used();
    let visible = view.visible_pages().len();
    let page_bytes = full / view.page_count().max(1) as usize;
    println!(
        "  cache after scrolling: {:.1} MiB for {} pages, {visible} visible",
        full as f64 / f64::from(1 << 20),
        view.page_count()
    );
    assert!(
        full > 4 * visible * page_bytes.max(1),
        "the cache holds many pages"
    );

    // The window goes to the background: another window takes the focus.
    f.state.set_trim_delay(Duration::from_millis(200));
    let other = gtk::Window::builder()
        .application(&common::application())
        .default_width(200)
        .default_height(100)
        .build();
    other.present();
    let inactive = pump_until_or(Duration::from_secs(5), || !f.state.window.is_active());
    println!("  the window became inactive: {inactive}");
    if inactive {
        let trimmed = pump_until_or(Duration::from_secs(5), || view.cache_used() < full / 2);
        assert!(
            trimmed,
            "the background window kept its whole PDF cache: {} bytes",
            view.cache_used()
        );
    }
    other.destroy();
    // At once, through the API.
    f.state.trim_memory();
    assert!(
        view.cache_used() <= visible * page_bytes * 2,
        "the PDF cache keeps more than the pages on screen: {} bytes",
        view.cache_used()
    );
}

/// This process's resident memory in kB.
fn rss_kb() -> i64 {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Rss:"))
                .and_then(|v| v.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

/// Opening the 60k-line `large.tex` costs tens of MiB; once its tab is closed most of that goes
/// back to the system (the view and buffer are finalized, and the allocator trims its free
/// pages). Before, every closed tab kept its view, buffer and the heap they had used.
fn regress2_closed_large_tab_gives_memory_back() {
    let large = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/large.tex"
    ))
    .unwrap();
    let f = fixture(
        "closed-large-tab",
        &[("main.tex", "x\n"), ("large.tex", &large)],
    );
    let _main = f.open("main.tex");
    common::pump_for(Duration::from_millis(300));
    // Free heap pages go back to the system (malloc_trim on glibc).
    lemma_app::heap::trim();
    let base = rss_kb();
    let editor = f.open("large.tex");
    let drawn = Rc::new(Cell::new(false));
    let flag = drawn.clone();
    editor.after_first_frame(move |_| flag.set(true));
    common::pump_until(TIMEOUT, || drawn.get());
    common::pump_for(Duration::from_millis(500));
    let open = rss_kb();
    f.state.close_editor(&editor, true);
    drop(editor);
    common::pump_for(lemma_app::window::CLOSED_TAB_TRIM_DELAY + Duration::from_millis(700));
    let closed = rss_kb();
    println!(
        "  RSS: +{:.1} MiB with large.tex open, +{:.1} MiB after its tab closed",
        (open - base) as f64 / 1024.0,
        (closed - base) as f64 / 1024.0
    );
    assert!(open - base > 8 << 10, "opening large.tex costs memory");
    assert!(
        (closed - base) * 2 < open - base,
        "closing the tab gave back less than half of what opening it took"
    );
}
