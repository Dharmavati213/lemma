//! WP3.5: the Preferences dialog (`prefs.rs`): every control round-trips to `config.toml`
//! without losing comments (also through a symbolic link), takes effect at once, follows edits
//! made elsewhere, and refuses what it cannot write.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_prefs`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `prefs-light.png` and `prefs-dark.png` there.

mod common;

use adw::prelude::*;
use lemma_app::editor::Editor;
use lemma_app::prefs::{self, Kind};
use lemma_app::settings::SettingsFiles;
use lemma_app::testmode;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    common::run(&[
        ("prefs_dialog_shows_every_setting", layout),
        ("prefs_every_control_round_trips_with_comments", round_trip),
        ("prefs_changes_take_effect_live", live),
        ("prefs_follow_edits_made_elsewhere", follow),
        ("prefs_numbers_are_written_once_settled", debounce),
        ("prefs_refuse_what_they_cannot_write", refusals),
        ("prefs_open_config_file", open_file),
        ("prefs_demo_resolves", demo),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

const DOC: &str = "\\documentclass{article}\n\\begin{document}\nHello.\n\\end{document}\n";

/// A configuration with comments everywhere: on lines of their own, and at the end of lines
/// the dialog changes.
const COMMENTED: &str = "\
# My Lemma settings (kept in dotfiles).

[editor]
# The font I like.
font = \"Monospace 11\"   # do not change lightly
wrap = true # soft wrap
vim = false

# Typing helpers.
[typing]
autopair = true  # pairs
autosnippets = true

[history]
max_days = 30 # a month

[keys]
\"win.build\" = \"F5\" # the default
";

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
    /// The file the dialog writes (a symbolic link's target when `link` is set).
    target: PathBuf,
    /// The `config.toml` the app reads.
    config: PathBuf,
}

/// A window whose settings come from `dir/config/config.toml` (with `text`), a symbolic link to
/// `dir/dotfiles/lemma.toml` when `link`.
fn fixture(name: &str, text: &str, link: bool) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    std::fs::write(dir.join("main.tex"), DOC).unwrap();
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    let target = if link {
        let dotfiles = dir.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("lemma.toml");
        std::fs::write(&target, text).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &config).unwrap();
        target
    } else {
        std::fs::write(&config, text).unwrap();
        config.clone()
    };
    let mut parsed = lemma_config::parse(text).unwrap_or_default();
    // The window starts without Vim and spell checking (then takes the file's values, as the
    // app does: `editor.vim` only decides new windows).
    parsed.editor.vim = false;
    parsed.spell.enabled = false;
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(parsed), None, ws, true);
    state.set_session_autosave(Some(false));
    state.set_settings_files(Some(SettingsFiles::next_to(config.clone())));
    // What the file says is the window's configuration (as the app starts).
    if let Ok(c) = lemma_config::parse(text) {
        state.set_config(Rc::new(c));
    }
    state.window.set_default_size(1000, 760);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture {
        state,
        dir,
        target,
        config,
    }
}

impl Fixture {
    fn open(&self, file: &str) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join(file))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        editor
    }

    fn show(&self) -> adw::PreferencesDialog {
        prefs::show(&self.state);
        let dialog = prefs::dialog(&self.state).expect("the dialog");
        common::pump_until(TIMEOUT, || dialog.is_mapped());
        dialog
    }

    fn text(&self) -> String {
        std::fs::read_to_string(&self.target).unwrap()
    }

    fn file_config(&self) -> Config {
        lemma_config::parse(&self.text()).expect("a valid configuration")
    }

    fn close(&self) {
        if let Some(d) = prefs::dialog(&self.state) {
            d.force_close();
        }
        common::pump_until(TIMEOUT, || prefs::dialog(&self.state).is_none());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(d) = self.state.window.visible_dialog() {
            d.force_close();
        }
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

/// Every widget inside `widget`, depth first.
fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![widget.clone()];
    while let Some(w) = stack.pop() {
        let mut child = w.last_child();
        while let Some(c) = child {
            child = c.prev_sibling();
            stack.push(c);
        }
        out.push(w);
    }
    out
}

/// The comments of `text`: whole-line comments and the comments at the end of lines.
fn comments(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.find('#').map(|i| l[i..].to_owned()))
        .collect()
}

fn layout() {
    let f = fixture("layout", COMMENTED, false);
    let dialog = f.show();
    // (The dialog's own search results page has no title.)
    let pages: Vec<adw::PreferencesPage> = descendants(dialog.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<adw::PreferencesPage>().ok())
        .filter(|p| !p.title().is_empty())
        .collect();
    let titles: Vec<String> = pages.iter().map(|p| p.title().to_string()).collect();
    let wanted: Vec<&str> = prefs::PAGES.iter().map(|p| p.title).collect();
    assert_eq!(titles, wanted);
    for (page, spec) in pages.iter().zip(prefs::PAGES) {
        assert_eq!(page.icon_name().as_deref(), Some(spec.icon));
        let rows: Vec<String> = descendants(page.upcast_ref())
            .iter()
            .filter_map(|w| w.downcast_ref::<adw::PreferencesRow>())
            .map(|r| r.title().to_string())
            .collect();
        for group in spec.groups {
            for setting in group.settings {
                assert!(
                    rows.iter().any(|r| r == setting.title),
                    "{} on {}: {rows:?}",
                    setting.title,
                    spec.title
                );
            }
        }
    }
    // The spec's list: font, Vim, wrap, build on save, dark PDF, spelling and its language,
    // typing helpers, completion, history limits, notifications, outline, math preview.
    let keys: Vec<(&str, &str)> = prefs::settings().map(|s| (s.section, s.key)).collect();
    for key in [
        ("editor", "font"),
        ("editor", "vim"),
        ("editor", "wrap"),
        ("build", "on_save"),
        ("pdf", "dark_invert"),
        ("spell", "enabled"),
        ("spell", "language"),
        ("typing", "autopair"),
        ("typing", "smart_dollar"),
        ("typing", "auto_end"),
        ("typing", "item_on_enter"),
        ("typing", "autosnippets"),
        ("completion", "auto"),
        ("history", "max_mib"),
        ("history", "max_days"),
        ("agent", "notify_on_turn_end"),
        ("outline", "follow_cursor"),
        ("preview", "math_hover"),
    ] {
        assert!(keys.contains(&key), "{key:?}");
    }
    // The configuration file row, with its path and Open button, on the first page.
    let first = descendants(pages[0].upcast_ref());
    let path_row = first
        .iter()
        .filter_map(|w| w.downcast_ref::<adw::ActionRow>())
        .find(|r| r.title() == "config.toml")
        .expect("the configuration file row");
    assert_eq!(
        path_row.subtitle().as_deref(),
        Some(f.config.to_str().unwrap())
    );
    assert!(first.iter().any(|w| {
        w.downcast_ref::<gtk::Button>()
            .is_some_and(|b| b.label().as_deref() == Some("_Open") && b.is_sensitive())
    }));
    // Opening it again keeps the one dialog.
    prefs::show(&f.state);
    assert_eq!(prefs::dialog(&f.state).as_ref(), Some(&dialog));
    f.close();
}

fn round_trip() {
    let f = fixture("round-trip", COMMENTED, true);
    let original_comments = comments(COMMENTED);
    let _editor = f.open("main.tex");
    f.show();
    let mut checked = 0;
    for setting in prefs::settings() {
        let (section, key) = (setting.section, setting.key);
        let before = f.state.config.get();
        match setting.kind {
            Kind::Bool(get) => {
                let wanted = !get(&before);
                assert_eq!(prefs::bool_value(&f.state, section, key), Some(!wanted));
                assert!(
                    prefs::set_bool(&f.state, section, key, wanted),
                    "{section}.{key}"
                );
                assert_eq!(get(&f.file_config()), wanted, "{section}.{key} in the file");
                assert_eq!(get(&f.state.config), wanted, "{section}.{key} applied");
                // And back.
                assert!(prefs::set_bool(&f.state, section, key, !wanted));
                assert_eq!(get(&f.file_config()), !wanted, "{section}.{key} back");
                assert_eq!(get(&f.state.config), !wanted);
            }
            Kind::Number {
                get,
                min,
                max,
                step,
            } => {
                let old = get(&before);
                let wanted = if old + step <= max { old + step } else { min };
                assert_eq!(prefs::number_value(&f.state, section, key), Some(old));
                assert!(prefs::set_number(&f.state, section, key, wanted));
                prefs::flush(&f.state);
                assert_eq!(get(&f.file_config()), wanted, "{section}.{key} in the file");
                assert_eq!(get(&f.state.config), wanted, "{section}.{key} applied");
            }
            Kind::Font => {
                assert_eq!(
                    prefs::font_value(&f.state).as_deref(),
                    Some(lemma_config::DEFAULT_FONT)
                );
                assert!(prefs::set_font(&f.state, "DejaVu Sans Mono 13"));
                assert_eq!(f.file_config().editor.font, "DejaVu Sans Mono 13");
                assert_eq!(f.state.config.editor.font, "DejaVu Sans Mono 13");
            }
            Kind::Language => {
                let (current, codes) = prefs::language_value(&f.state).unwrap();
                assert_eq!(current, before.spell.language);
                let other = codes.iter().find(|c| **c != current).cloned();
                match other {
                    Some(other) => {
                        assert!(prefs::set_language(&f.state, &other));
                        assert_eq!(f.file_config().spell.language, other);
                        assert_eq!(f.state.config.spell.language, other);
                        assert!(prefs::set_language(&f.state, &current));
                        assert_eq!(f.file_config().spell.language, current);
                    }
                    None => println!("    (one dictionary installed: the language stays)"),
                }
            }
        }
        // Nothing else changed: every comment is still there, in order.
        let text = f.text();
        assert_eq!(
            comments(&text),
            original_comments,
            "after {section}.{key}:\n{text}"
        );
        checked += 1;
    }
    assert_eq!(checked, prefs::settings().count());
    // Lines the dialog changed keep their comments; the link is still a link.
    let text = f.text();
    assert!(
        text.contains("font = \"DejaVu Sans Mono 13\"   # do not change lightly\n"),
        "{text}"
    );
    assert!(text.contains("max_days = 31 # a month\n"), "{text}");
    assert!(
        text.starts_with(
            "# My Lemma settings (kept in dotfiles).\n\n[editor]\n# The font I like.\n"
        )
    );
    assert!(text.contains("\"win.build\" = \"F5\" # the default\n"));
    assert!(std::fs::symlink_metadata(&f.config).unwrap().is_symlink());
    assert_eq!(std::fs::read_to_string(&f.config).unwrap(), text);
    // The whole file means what the window uses.
    assert_eq!(f.file_config(), *f.state.config.get());
    f.close();
}

/// Builds on save off (the tests save nothing, but a build would be noise); no spell checking
/// or Vim at first.
const LIVE: &str =
    "[build]\non_save = false\n\n[spell]\nenabled = false\n\n[editor]\nvim = false\n";

fn live() {
    let f = fixture("live", LIVE, false);
    let editor = f.open("main.tex");
    f.show();
    // Wrapping: every editor at once.
    assert_eq!(editor.view().wrap_mode(), gtk::WrapMode::WordChar);
    assert!(prefs::set_bool(&f.state, "editor", "wrap", false));
    assert_eq!(editor.view().wrap_mode(), gtk::WrapMode::None);
    // Vim: switched in this window too.
    assert!(!f.state.vim_enabled());
    assert!(prefs::set_bool(&f.state, "editor", "vim", true));
    assert!(f.state.vim_enabled() && editor.vim().is_some());
    assert!(prefs::set_bool(&f.state, "editor", "vim", false));
    assert!(!f.state.vim_enabled() && editor.vim().is_none());
    // The PDF strip.
    assert!(!f.state.pdf_sidebar.split.shows_sidebar());
    assert!(prefs::set_bool(&f.state, "pdf", "thumbnails", true));
    assert!(f.state.pdf_sidebar.split.shows_sidebar());
    // Spell checking starts and stops.
    assert!(prefs::set_bool(&f.state, "spell", "enabled", true));
    common::pump_until(TIMEOUT, || lemma_app::editor::spell::is_checking(&editor));
    assert!(prefs::set_bool(&f.state, "spell", "enabled", false));
    common::pump_until(TIMEOUT, || !lemma_app::editor::spell::is_checking(&editor));
    // The rest is read where it is used: the window's configuration has it.
    assert!(prefs::set_bool(&f.state, "typing", "autopair", false));
    assert!(!f.state.config.typing.autopair);
    assert!(prefs::set_bool(&f.state, "outline", "follow_cursor", false));
    assert!(!f.state.config.outline.follow_cursor);
    assert!(prefs::set_number(&f.state, "preview", "delay_ms", 500));
    prefs::flush(&f.state);
    assert_eq!(f.state.config.preview.delay_ms, 500);
    // The font: the editor CSS follows (the configuration has it; the shell installs it).
    assert!(prefs::set_font(&f.state, "Monospace 14"));
    assert_eq!(f.state.config.editor.font, "Monospace 14");
    // The new keys went into their sections.
    let text = f.text();
    assert!(text.starts_with("[build]\non_save = false\n"), "{text}");
    assert!(text.contains("[pdf]\nthumbnails = true\n"), "{text}");
    f.close();
}

fn follow() {
    let f = fixture("follow", COMMENTED, false);
    f.show();
    assert_eq!(prefs::bool_value(&f.state, "editor", "wrap"), Some(true));
    assert_eq!(
        prefs::number_value(&f.state, "history", "max_days"),
        Some(30)
    );
    // Another program changes the file; the live reload applies it.
    let edited = COMMENTED
        .replace("wrap = true", "wrap = false")
        .replace("max_days = 30", "max_days = 7")
        .replace("font = \"Monospace 11\"", "font = \"Monospace 9\"");
    std::fs::write(&f.target, &edited).unwrap();
    f.state
        .set_config(Rc::new(lemma_config::parse(&edited).unwrap()));
    assert_eq!(prefs::bool_value(&f.state, "editor", "wrap"), Some(false));
    assert_eq!(
        prefs::number_value(&f.state, "history", "max_days"),
        Some(7)
    );
    assert_eq!(prefs::font_value(&f.state).as_deref(), Some("Monospace 9"));
    common::pump_for(prefs::NUMBER_DEBOUNCE + Duration::from_millis(100));
    prefs::flush(&f.state);
    assert_eq!(f.text(), edited, "following the file writes nothing back");
    // Closing disconnects: a later change does not reach the closed dialog.
    f.close();
    f.state
        .set_config(Rc::new(lemma_config::parse(COMMENTED).unwrap()));
    assert_eq!(f.text(), edited);
}

fn debounce() {
    let f = fixture("debounce", COMMENTED, false);
    f.show();
    for days in [31, 32, 33, 40] {
        assert!(prefs::set_number(&f.state, "history", "max_days", days));
        common::pump_for(Duration::from_millis(30));
    }
    assert_eq!(f.text(), COMMENTED, "nothing written while changing");
    assert_eq!(f.state.config.history.max_days, 30);
    common::pump_until(TIMEOUT, || f.file_config().history.max_days == 40);
    assert_eq!(f.state.config.history.max_days, 40);
    // Closing writes what waits.
    assert!(prefs::set_number(&f.state, "history", "max_mib", 300));
    f.close();
    assert_eq!(f.file_config().history.max_mib, 300);
    assert!(f.text().contains("max_days = 40 # a month\n"));
}

fn toasts(dialog: &adw::PreferencesDialog) -> Vec<String> {
    descendants(dialog.upcast_ref())
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Label>())
        .filter(|l| l.ancestor(adw::ToastOverlay::static_type()).is_some())
        .map(|l| l.label().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn refusals() {
    // An inline table the line editor cannot follow: refused, the switch goes back.
    let inline = "# mine\neditor = { wrap = false }\n";
    let f = fixture("refuse-inline", inline, false);
    let dialog = f.show();
    assert_eq!(prefs::bool_value(&f.state, "editor", "wrap"), Some(false));
    assert!(prefs::set_bool(&f.state, "editor", "wrap", true));
    assert_eq!(f.text(), inline, "nothing written");
    assert_eq!(
        prefs::bool_value(&f.state, "editor", "wrap"),
        Some(false),
        "reverted"
    );
    assert!(!f.state.config.editor.wrap);
    common::pump_until(TIMEOUT, || !toasts(&dialog).is_empty());
    let shown = toasts(&dialog).join(" ");
    assert!(
        shown.contains("Could not change “Wrap Long Lines”"),
        "{shown}"
    );
    f.close();
    drop(f);

    // LEMMA_CONFIG=/dev/null: nothing can be written; the dialog says so.
    let f = fixture("refuse-null", "", false);
    f.state
        .set_settings_files(Some(SettingsFiles::next_to("/dev/null".into())));
    assert!(!prefs::is_writable(&f.state));
    let dialog = f.show();
    let widgets = descendants(dialog.upcast_ref());
    let groups: Vec<&adw::PreferencesGroup> = widgets
        .iter()
        .filter_map(|w| w.downcast_ref::<adw::PreferencesGroup>())
        .collect();
    let titles: Vec<&str> = prefs::PAGES
        .iter()
        .flat_map(|p| p.groups.iter().map(|g| g.title))
        .collect();
    let controls: Vec<&&adw::PreferencesGroup> = groups
        .iter()
        .filter(|g| titles.contains(&g.title().as_str()))
        .collect();
    assert!(controls.len() >= titles.len() - 1, "{}", controls.len());
    assert!(
        controls.iter().all(|g| !g.is_sensitive()),
        "the controls are insensitive"
    );
    assert!(groups.iter().any(|g| {
        g.description()
            .is_some_and(|d| d.contains("cannot be changed here"))
    }));
    // A change anyway is refused and reverted.
    let before = f.state.config.get();
    assert!(prefs::set_bool(&f.state, "editor", "wrap", false));
    assert_eq!(prefs::bool_value(&f.state, "editor", "wrap"), Some(true));
    assert_eq!(*f.state.config.get(), *before);
    common::pump_until(TIMEOUT, || !toasts(&dialog).is_empty());
    let shown = toasts(&dialog).join(" ");
    assert!(shown.contains("not a file"), "{shown}");
    f.close();
}

fn open_file() {
    let f = fixture("open-file", COMMENTED, false);
    let opened: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let log = opened.clone();
    prefs::set_open_hook(Some(Box::new(move |p: &Path| {
        log.borrow_mut().push(p.into())
    })));
    prefs::open_file(&f.state);
    assert_eq!(*opened.borrow(), std::slice::from_ref(&f.config));
    assert_eq!(f.text(), COMMENTED, "an existing file is left alone");
    // A missing file is created first (with a comment that says what it is).
    let fresh = f.dir.join("fresh/config.toml");
    f.state
        .set_settings_files(Some(SettingsFiles::next_to(fresh.clone())));
    prefs::open_file(&f.state);
    assert_eq!(opened.borrow().last(), Some(&fresh));
    let text = std::fs::read_to_string(&fresh).unwrap();
    assert!(text.starts_with("# "), "{text}");
    assert_eq!(lemma_config::parse(&text).unwrap(), Config::default());
    prefs::set_open_hook(None);
}

/// Saves a snapshot of the window when `LEMMA_TEST_SNAPSHOTS` names a directory.
fn snapshot(f: &Fixture, name: &str) {
    if let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") {
        let path = PathBuf::from(dir).join(name);
        block_on(testmode::snapshot_next_frame(&f.state.window, &path)).unwrap();
    }
}

fn demo() {
    let f = fixture("demo", COMMENTED, false);
    let _editor = f.open("main.tex");
    block_on(prefs::run_demo(f.state.clone())).unwrap();
    assert!(prefs::dialog(&f.state).is_some_and(|d| d.is_mapped()));
    snapshot(&f, "prefs-light.png");
    let style = adw::StyleManager::default();
    style.set_color_scheme(adw::ColorScheme::ForceDark);
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
    snapshot(&f, "prefs-dark.png");
    style.set_color_scheme(adw::ColorScheme::Default);
    // The other pages, and the end of the first one (the configuration file).
    let dialog = prefs::dialog(&f.state).unwrap();
    let pages: Vec<adw::PreferencesPage> = descendants(dialog.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<adw::PreferencesPage>().ok())
        .filter(|p| !p.title().is_empty())
        .collect();
    if std::env::var_os("LEMMA_TEST_SNAPSHOTS").is_some() {
        if let Some(scrolled) = descendants(pages[0].upcast_ref())
            .into_iter()
            .find_map(|w| w.downcast::<gtk::ScrolledWindow>().ok())
        {
            let adj = scrolled.vadjustment();
            adj.set_value(adj.upper());
        }
        assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
        snapshot(&f, "prefs-editor-end.png");
        for (page, name) in pages[1..]
            .iter()
            .zip(["prefs-preview.png", "prefs-agent.png"])
        {
            dialog.set_visible_page(page);
            assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
            snapshot(&f, name);
        }
    }
    assert_eq!(f.text(), COMMENTED, "opening writes nothing");
    f.close();
}
