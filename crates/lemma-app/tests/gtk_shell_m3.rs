//! The M3 shell hooks (WP3.0): the hover and goto registries, the back-jump list, the new
//! actions and their accelerators (also on real key events), the Search page, the PDF strip,
//! the status-bar slot, the session hooks and editing `config.toml` for Preferences.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_shell_m3`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `m3-hover-light.png` and `m3-hover-dark.png` (a hover
//! popover from a test provider) there.

mod broadway_input;
mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::app::{self, ShortcutGroup};
use lemma_app::editor::goto::{self, GotoTarget};
use lemma_app::editor::hover::{self, HoverContent};
use lemma_app::editor::keys::{self, KeyEvent};
use lemma_app::editor::{Editor, TextPoint, Trigger, VimMode};
use lemma_app::settings::SettingsFiles;
use lemma_app::testmode;
use lemma_app::window::{PAGE_OUTLINE, PAGE_SEARCH, WindowState};
use lemma_app::workspace::{Workspace, normalize};
use lemma_config::{Config, ConfigValue};
use lemma_core::ProjectRoot;
use lemma_syntax::{Context, TokenKind};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    if broadway_input::is_child() {
        return common::run(&[("m3_real_keys_child", real_keys::child)]);
    }
    common::run(&[
        ("m3_actions_have_their_default_accelerators", actions),
        ("m3_accelerators_follow_keys_overrides", accel_overrides),
        (
            "no_accelerator_is_shared_or_taken_from_typing",
            accel_collisions,
        ),
        ("main_menu_lists_the_m3_entries", menu_entries),
        ("sidebar_search_page_and_status_slot", sidebar_and_status),
        ("pdf_strip_slot_toggle_and_config", pdf_strip),
        (
            "pdf_search_enabled_only_with_the_pdf_focused",
            pdf_search_focus,
        ),
        ("text_point_names_under_the_point", text_point),
        ("hover_providers_by_priority", hover_priority),
        ("hover_markup_popover_keeps_the_focus", hover_markup),
        ("hover_pointer_dwell_range_and_leaving", hover_pointer),
        ("hover_hides_on_typing_and_scrolling", hover_hiding),
        ("hover_pending_latest_wins_and_spinner", hover_pending),
        ("hover_keyboard_alt_k_and_vim_k", hover_keyboard),
        ("hover_invalid_markup_is_plain_text", hover_invalid_markup),
        ("hover_and_goto_detach_with_the_tab", registries_detach),
        ("goto_resolvers_by_priority", goto_priority),
        ("goto_definition_back_and_forward", goto_jumps),
        ("goto_back_follows_edits_and_is_capped", goto_marks_and_cap),
        ("goto_vim_gd_and_ctrl_o", goto_vim),
        ("goto_ctrl_click_and_link_underline", goto_pointer),
        ("goto_line_pdf_and_external_targets", goto_targets),
        ("goto_back_handlers_run_first", goto_back_handlers),
        ("session_saves_are_debounced", session_hooks),
        ("edit_config_file_keeps_comments_and_applies", edit_config),
        ("m3_stub_actions_are_wired", stubs_wired),
        ("m3_keys_on_real_key_events", real_keys::parent),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

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
    config.preview.delay_ms = 60;
    // The registry tests use their own providers (WP3.3's real math preview is off).
    config.preview.math_hover = false;
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
        // The registries are tested with their own providers and resolvers (WP3.2's are tested
        // in gtk_goto.rs).
        lemma_app::labels::remove_tools(&editor);
        // Laid out and drawn: positions on screen exist.
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        assert!(block_on(testmode::wait_for_frames(&self.state.window, 1)));
        editor
    }

    fn path(&self, file: &str) -> PathBuf {
        normalize(&self.dir.join(file))
    }

    fn activate(&self, action: &str) {
        WidgetExt::activate_action(&self.state.window, action, None).unwrap();
    }

    fn active_path(&self) -> Option<PathBuf> {
        self.state
            .active_editor()
            .map(|e| e.document().path().to_path_buf())
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

/// The middle of the character at `offset`, in the view's widget coordinates.
fn point_of(editor: &Editor, offset: usize) -> (f64, f64) {
    let view = editor.view();
    let iter = editor.iter_near(offset).unwrap();
    let rect = view.iter_location(&iter);
    let (x, y) = view.buffer_to_window_coords(
        gtk::TextWindowType::Widget,
        rect.x() + rect.width() / 2,
        rect.y() + rect.height() / 2,
    );
    let point = (f64::from(x), f64::from(y));
    assert_eq!(
        editor.offset_at_point(point.0, point.1),
        Some(offset),
        "the point {point:?} is over byte {offset}"
    );
    point
}

/// Every pixel column of the character at `offset` maps back to it, both halves (the edge
/// columns are shared with the neighbours: positions are fractional).
fn assert_character_columns(editor: &Editor, offset: usize) {
    let view = editor.view();
    let rect = view.iter_location(&editor.iter_near(offset).unwrap());
    for bx in rect.x() + 1..rect.x() + rect.width() - 1 {
        let (x, y) = view.buffer_to_window_coords(
            gtk::TextWindowType::Widget,
            bx,
            rect.y() + rect.height() / 2,
        );
        assert_eq!(
            editor.offset_at_point(f64::from(x), f64::from(y)),
            Some(offset),
            "column {bx} of the character at byte {offset}"
        );
    }
}

fn set_cursor(editor: &Editor, offset: usize) {
    assert!(editor.place_cursor_at(offset));
}

fn label_text(widget: &gtk::Widget) -> String {
    widget
        .downcast_ref::<gtk::Label>()
        .map(|l| l.text().to_string())
        .unwrap_or_default()
}

const DOC: &str = "\\documentclass{article}\n\
\\newcommand{\\cF}{\\mathcal{F}}\n\
\\begin{document}\n\
See \\cref{eq:a, eq:b} and \\cite{knuth}.\n\
$\\cF \\to \\Hom$ text \\\\word\n\
\\input{chapters/a}\n\
\\end{document}\n";

const CHAPTER: &str = "\\section{A}\n\\label{sec:a}\nText of chapter A.\n";

// ----- actions, accelerators, menus, slots -----

/// The M3 entries of `app::DEFAULT_ACCELS`.
fn m3_accels() -> Vec<(&'static str, &'static [&'static str])> {
    let first = app::DEFAULT_ACCELS
        .iter()
        .position(|(a, _)| *a == "win.command-palette")
        .unwrap();
    app::DEFAULT_ACCELS[first..].to_vec()
}

fn same_accels(got: &[glib::GString], want: &[&str]) -> bool {
    let parse = |a: &str| gtk::accelerator_parse(a).expect("an accelerator");
    got.len() == want.len() && got.iter().zip(want).all(|(g, w)| parse(g) == parse(w))
}

fn actions() {
    let f = fixture("actions", &[]);
    app::set_accels(&f.state.app, &f.state.config.keys);
    let expected = [
        ("win.command-palette", "<Control><Shift>p"),
        ("win.quick-open", "<Control>p"),
        ("win.project-search", "<Control><Shift>f"),
        ("win.shortcuts", "<Control>question"),
        ("win.preferences", "<Control>comma"),
        ("win.pdf-search", "<Control>f"),
        ("win.goto-definition", "F12"),
        ("win.find-references", "<Shift>F12"),
        ("win.rename-label", "F2"),
        ("win.jump-pair", "<Control>percent"),
        ("win.goto-back", "<Alt>Left"),
        ("win.goto-forward", "<Alt>Right"),
        ("win.show-hover", "<Alt>k"),
    ];
    assert_eq!(m3_accels().len(), expected.len());
    for (action, accel) in expected {
        let got = f.state.app.accels_for_action(action);
        assert!(same_accels(&got, &[accel]), "{action}: {got:?}");
        assert!(app::action_info(action).is_some(), "{action} is described");
    }
    for name in [
        "command-palette",
        "quick-open",
        "project-search",
        "shortcuts",
        "preferences",
        "pdf-search",
        "pdf-detach",
        "goto-definition",
        "goto-back",
        "goto-forward",
        "show-hover",
        "find-references",
        "rename-label",
        "jump-pair",
        "history",
        "word-count",
        "toggle-pdf-sidebar",
    ] {
        assert!(f.state.window.lookup_action(name).is_some(), "win.{name}");
        assert!(app::action_info(&format!("win.{name}")).is_some(), "{name}");
    }
    // Groups for the shortcuts window.
    assert_eq!(
        app::action_info("win.goto-definition").unwrap().group,
        ShortcutGroup::Navigation
    );
    assert_eq!(
        app::action_info("win.pdf-search").unwrap().group,
        ShortcutGroup::BuildPdf
    );
    // The Vim bindings of M3.
    let vim = f.state.config.vim.normal_bindings();
    assert_eq!(vim["gd"], "win.goto-definition");
    assert_eq!(vim["K"], "win.show-hover");
}

/// WP3.10: no key runs two actions; every accelerator has Ctrl or Alt or is a function key (a
/// plain or Shift+ key would be taken from typing and from Vim); Ctrl+W and Ctrl+O stay Close Tab
/// and Open (§11: in the editor they go to Vim); the keys the panes handle themselves (the Search
/// page, the PDF view and its search bar, the pickers, context menus) are no accelerator, which
/// would run first; and the `[vim.normal]` defaults neither shadow each other nor take the
/// key-layer handlers' `%` and Ctrl+O.
fn accel_collisions() {
    let f = fixture("accel-collisions", &[("main.tex", DOC)]);
    let _editor = f.open("main.tex");
    app::set_accels(&f.state.app, &f.state.config.keys);
    let mut owners: std::collections::HashMap<(gdk::Key, gdk::ModifierType), String> =
        std::collections::HashMap::new();
    let actions = f.state.app.list_action_descriptions();
    // Every window action with a default key (39 when this was written).
    assert!(actions.len() > 30, "{actions:?}");
    for action in &actions {
        for accel in f.state.app.accels_for_action(action) {
            let (key, mods) = gtk::accelerator_parse(&accel)
                .unwrap_or_else(|| panic!("{action}: {accel} does not parse"));
            if let Some(other) = owners.insert((key.to_lower(), mods), action.to_string()) {
                panic!("{accel} runs both {other} and {action}");
            }
            let function_key = key
                .name()
                .is_some_and(|n| n.strip_prefix('F').is_some_and(|d| d.parse::<u8>().is_ok()));
            let modified =
                mods.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK);
            assert!(
                modified || function_key,
                "{action}: {accel} would be taken from typing"
            );
        }
    }
    let owner = |accel: &str| {
        let (key, mods) = gtk::accelerator_parse(accel).unwrap();
        owners.get(&(key.to_lower(), mods)).cloned()
    };
    assert_eq!(owner("<Control>w").as_deref(), Some("win.close-tab"));
    assert_eq!(owner("<Control>o").as_deref(), Some("app.open"));
    for local in [
        // The Search page.
        "<Alt>c",
        "<Alt>w",
        "<Alt>r",
        "<Control>h",
        "<Control><Alt>Return",
        "<Control>Return",
        // The PDF view, its search bar and the detached PDF window's Ctrl+F (a window of its own).
        "<Control>g",
        "<Control><Shift>g",
        "<Control>plus",
        "<Control>equal",
        "<Control>minus",
        "<Control>0",
        "BackSpace",
        "slash",
        "Escape",
        // The pickers (palette, quick open) and context menus; completion.
        "<Control>n",
        "<Control>j",
        "<Shift>F10",
        "Menu",
        "<Control>space",
    ] {
        assert_eq!(owner(local), None, "{local} is a pane's own key");
    }
    let bindings: Vec<&str> = lemma_config::DEFAULT_VIM_NORMAL
        .iter()
        .map(|(keys, _)| *keys)
        .collect();
    for a in &bindings {
        for b in &bindings {
            assert!(a == b || !b.starts_with(a), "{a} shadows {b}");
        }
        assert!(
            !a.starts_with('%') && !a.to_lowercase().starts_with("<c-o>"),
            "{a} takes a key-layer key"
        );
    }
    for (_, action) in lemma_config::DEFAULT_VIM_NORMAL {
        let name = action.strip_prefix("win.").unwrap();
        assert!(f.state.window.lookup_action(name).is_some(), "{action}");
    }
}

fn accel_overrides() {
    let f = fixture("accel-overrides", &[]);
    let app = &f.state.app;
    let mut keys = lemma_config::KeysCfg::default();
    keys.bindings
        .insert("win.quick-open".into(), "<Control><Alt>p".into());
    keys.bindings.insert("win.goto-back".into(), String::new());
    app::set_accels(app, &keys);
    assert!(same_accels(
        &app.accels_for_action("win.quick-open"),
        &["<Control><Alt>p"]
    ));
    assert!(app.accels_for_action("win.goto-back").is_empty());
    // A reload without the overrides brings the defaults back.
    app::reapply_accels(app, &keys, &lemma_config::KeysCfg::default());
    assert!(same_accels(
        &app.accels_for_action("win.quick-open"),
        &["<Control>p"]
    ));
    assert!(same_accels(
        &app.accels_for_action("win.goto-back"),
        &["<Alt>Left"]
    ));
    assert_eq!(
        app::default_accels("win.show-hover"),
        Some(vec!["<Alt>k".to_owned()])
    );
}

/// Every (label, action) of `menu` and its sections.
fn menu_items(menu: &gio::MenuModel) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for i in 0..menu.n_items() {
        let label = menu
            .item_attribute_value(i, "label", Some(glib::VariantTy::STRING))
            .and_then(|v| v.get::<String>());
        let action = menu
            .item_attribute_value(i, "action", Some(glib::VariantTy::STRING))
            .and_then(|v| v.get::<String>());
        if let (Some(label), Some(action)) = (label, action) {
            out.push((label, action));
        }
        if let Some(section) = menu.item_link(i, "section") {
            out.extend(menu_items(&section));
        }
    }
    out
}

fn menu_entries() {
    let f = fixture("menu", &[]);
    let items = menu_items(f.state.header.menu.upcast_ref());
    let actions: Vec<&str> = items.iter().map(|(_, a)| a.as_str()).collect();
    for action in [
        "win.command-palette",
        "win.quick-open",
        "win.project-search",
        "win.pdf-detach",
        "win.preferences",
        "win.shortcuts",
        "app.quit",
    ] {
        assert!(actions.contains(&action), "{action} in {actions:?}");
    }
    // Command Palette, Go to File, Find in Project, and WP3.2's Unused Labels and Entries.
    assert_eq!(f.state.header.menu_go.n_items(), 4);
    // Preferences and Keyboard Shortcuts come right before Quit, as GNOME apps have them.
    let at = |a: &str| actions.iter().position(|x| *x == a).unwrap();
    assert!(at("win.preferences") < at("win.shortcuts"));
    assert_eq!(at("win.shortcuts") + 1, at("app.quit"));
}

fn sidebar_and_status() {
    let f = fixture("sidebar", &[]);
    let sidebar = &f.state.sidebar;
    let search = sidebar
        .stack
        .child_by_name(PAGE_SEARCH)
        .expect("a Search page");
    assert_eq!(&search, sidebar.search.upcast_ref::<gtk::Widget>());
    for (page, icon) in [
        (&sidebar.outline, "view-list-bullet-symbolic"),
        (&sidebar.changes, "document-edit-symbolic"),
        (&sidebar.problems, "dialog-warning-symbolic"),
        (&sidebar.search, "edit-find-symbolic"),
    ] {
        let page = sidebar.stack.page(page);
        assert_eq!(page.icon_name().as_deref(), Some(icon));
        assert!(page.title().is_some());
    }
    // Ctrl+Shift+F shows it (the stub shows the placeholder page).
    sidebar.show_page(PAGE_OUTLINE);
    f.activate("win.project-search");
    assert_eq!(
        sidebar.stack.visible_child_name().as_deref(),
        Some(PAGE_SEARCH)
    );
    assert!(sidebar.split.shows_sidebar());
    // The word-count slot sits in the status bar, hidden until WP3.7 shows it.
    let slot = &f.state.status.word_count;
    assert_eq!(
        slot.parent().as_ref(),
        Some(f.state.status.bar.upcast_ref::<gtk::Widget>())
    );
    assert!(!slot.is_visible());
    assert_eq!(
        slot.next_sibling().as_ref(),
        Some(f.state.status.position.upcast_ref::<gtk::Widget>())
    );
}

fn pdf_strip() {
    let f = fixture("pdf-strip", &[]);
    let strip = &f.state.pdf_sidebar;
    assert!(!strip.split.shows_sidebar(), "[pdf] thumbnails is off");
    assert!(
        strip.split.is_ancestor(&f.state.pdf_pane),
        "the strip is inside the PDF pane"
    );
    let content = strip.split.content().expect("the PDF view's scroller");
    let view = f.state.pdf.get().unwrap().view().clone();
    assert!(view.is_ancestor(&content));
    assert!(strip.slot.is_ancestor(&strip.split));
    // The toggle is the first button of the PDF toolbar.
    let toolbar = strip.toggle.parent().expect("in the toolbar");
    assert!(toolbar.has_css_class("lemma-pdf-toolbar"));
    assert_eq!(
        toolbar.first_child().as_ref(),
        Some(strip.toggle.upcast_ref::<gtk::Widget>())
    );
    f.activate("win.toggle-pdf-sidebar");
    assert!(strip.split.shows_sidebar());
    f.activate("win.toggle-pdf-sidebar");
    assert!(!strip.split.shows_sidebar());
    // `[pdf] thumbnails` decides at start and when it changes.
    let mut config = (*f.state.config.get()).clone();
    config.pdf.thumbnails = true;
    f.state.set_config(Rc::new(config.clone()));
    assert!(strip.split.shows_sidebar());
    strip.split.set_show_sidebar(false);
    config.editor.wrap = !config.editor.wrap;
    f.state.set_config(Rc::new(config));
    assert!(
        !strip.split.shows_sidebar(),
        "another change leaves the user's choice alone"
    );
    let g = fixture_with("pdf-strip-on", &[], |c| c.pdf.thumbnails = true);
    assert!(g.state.pdf_sidebar.split.shows_sidebar());
}

fn pdf_search_focus() {
    let f = fixture("pdf-search-focus", &[("main.tex", DOC)]);
    let pdf_file = f.dir.join("main.pdf");
    std::fs::copy(common::fixtures().join("proj/main.pdf"), &pdf_file).unwrap();
    let editor = f.open("main.tex");
    let enabled = || {
        f.state
            .window
            .lookup_action("pdf-search")
            .unwrap()
            .is_enabled()
    };
    editor.view().grab_focus();
    common::pump_for(Duration::from_millis(20));
    assert!(!enabled(), "Ctrl+F stays the editor's");
    let pdf = f.state.pdf.get().unwrap();
    pdf.open(&pdf_file);
    common::pump_until(TIMEOUT, || {
        pdf.pdf().is_some() && pdf.view().is_mapped() && pdf.view().height() > 0
    });
    assert!(pdf.view().grab_focus());
    common::pump_until(TIMEOUT, enabled);
    // A popover taking the focus (the command palette) leaves it as it was.
    let entry = gtk::Entry::new();
    let popover = gtk::Popover::builder().child(&entry).build();
    popover.set_parent(editor.view());
    popover.popup();
    entry.grab_focus();
    common::pump_for(Duration::from_millis(50));
    assert!(enabled(), "still the PDF's state behind the popover");
    popover.popdown();
    popover.unparent();
    editor.view().grab_focus();
    common::pump_until(TIMEOUT, || !enabled());
}

// ----- TextPoint -----

fn text_point() {
    let f = fixture("text-point", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    common::pump_until(TIMEOUT, || doc.has_line_states());
    let at = |needle: &str, delta: usize| {
        TextPoint::new(&doc, offset_of(&editor, needle, delta)).expect("a point")
    };

    // A reference list: the label under the point, blanks trimmed.
    let p = at("eq:b", 2);
    assert_eq!(p.token_kind(), Some(TokenKind::RefArg));
    let (range, name) = p.item().expect("an item");
    assert_eq!(name, "eq:b");
    assert_eq!(&DOC[range.clone()], "eq:b");
    assert_eq!(p.anchor(), range);
    assert!(matches!(p.context, Context::Arg { ref cmd, .. } if &**cmd == "cref"));
    assert_eq!(at("eq:a", 0).item().unwrap().1, "eq:a");
    assert_eq!(at(", eq:b", 0).item(), None, "the comma");
    assert_eq!(at(", eq:b", 1).item(), None, "the blank");
    assert_eq!(at("knuth", 4).item().unwrap().1, "knuth");
    let file = at("chapters/a", 3);
    assert_eq!(file.token_kind(), Some(TokenKind::FileArg));
    assert_eq!(file.item().unwrap().1, "chapters/a");
    // Control words: on a letter or the backslash.
    let cref = at("\\cref", 2);
    let (range, name) = cref.control_word().unwrap();
    assert_eq!((name, &DOC[range]), ("cref", "\\cref"));
    assert_eq!(at("\\cref", 0).control_word().unwrap().1, "cref");
    let math = at("\\cF \\to", 0);
    assert_eq!(math.control_word().unwrap().1, "cF");
    assert!(math.context.is_math());
    assert_eq!(math.token_kind(), Some(TokenKind::MathCommand));
    // `\\word` is a line break and a word.
    assert_eq!(at("\\\\word", 3).control_word(), None);
    assert_eq!(at("See", 1).control_word(), None);
    assert_eq!(at("See", 1).item(), None);
    // Line and column.
    let p = at("See", 0);
    assert_eq!((p.line, p.column), (3, 0));
    assert_eq!(p.line_text, "See \\cref{eq:a, eq:b} and \\cite{knuth}.");
    assert_eq!(p.line_start(), offset_of(&editor, "See", 0));
    // Past the text, or inside a character: no point.
    assert!(TextPoint::new(&doc, DOC.len()).is_some());
    assert!(TextPoint::new(&doc, DOC.len() + 1).is_none());
    editor
        .buffer()
        .insert(&mut editor.buffer().start_iter(), "é");
    assert!(TextPoint::new(&doc, 1).is_none(), "inside é");
}

// ----- hover -----

/// A provider answering `markup` for the label `name` of a reference list, counting its calls.
fn label_provider(
    name: &'static str,
    markup: &'static str,
    calls: Rc<Cell<u32>>,
) -> impl Fn(&hover::HoverCtx) -> Option<HoverContent> + 'static {
    move |ctx| {
        calls.set(calls.get() + 1);
        (ctx.item()?.1 == name).then(|| HoverContent::Markup(markup.to_owned()))
    }
}

fn hover_priority() {
    let f = fixture("hover-priority", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let view = editor.view();
    // WP3.2's providers are off (`Fixture::open`); other modules' (the math preview) stay.
    let base = hover::provider_count(view);
    let order: Rc<RefCell<Vec<&str>>> = Rc::default();
    let log = |name: &'static str, answer: bool| {
        let order = order.clone();
        move |_: &hover::HoverCtx| {
            order.borrow_mut().push(name);
            answer.then(|| HoverContent::text(name))
        }
    };
    let low = hover::register(view, 10, log("low", true)).unwrap();
    let high = hover::register(view, 300, log("high", false)).unwrap();
    let mid = hover::register(view, 100, log("mid", true)).unwrap();
    let _mid2 = hover::register(view, 100, log("mid2", true)).unwrap();
    assert_eq!(hover::provider_count(view), base + 4);
    let offset = offset_of(&editor, "See", 0);
    let ctx = hover::HoverCtx::new(
        TextPoint::new(editor.document(), offset).unwrap(),
        Trigger::Pointer,
    );
    let content = hover::query(view, &ctx).unwrap();
    assert!(matches!(content, HoverContent::Markup(ref m) if m == "mid"));
    assert_eq!(*order.borrow(), ["high", "mid"], "the first answer wins");
    hover::unregister(view, mid);
    hover::unregister(view, high);
    order.borrow_mut().clear();
    assert!(matches!(hover::query(view, &ctx), Some(HoverContent::Markup(m)) if m == "mid2"));
    hover::unregister(view, low);
    assert_eq!(hover::provider_count(view), base + 1);
}

fn hover_markup() {
    let f = fixture("hover-markup", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let view = editor.view();
    let calls = Rc::new(Cell::new(0));
    hover::register(
        view,
        hover::PRIORITY_REFS,
        label_provider(
            "eq:b",
            "<b>Equation (2.3)</b>, page 4\n<tt>\\label{eq:b}</tt>",
            calls,
        ),
    )
    .unwrap();
    view.grab_focus();
    common::pump_until(TIMEOUT, || view.has_focus());
    let offset = offset_of(&editor, "eq:b", 1);
    assert!(hover::show_at(&editor, offset, Trigger::Pointer));
    let popover = hover::popover(view).expect("created on the first hover");
    common::pump_until(TIMEOUT, || popover.is_visible() && popover.is_mapped());
    assert!(popover.has_css_class(hover::CSS_CLASS));
    assert!(!popover.is_focusable() && !popover.can_focus());
    assert!(!popover.is_autohide());
    let label = hover::shown_widget(view).expect("the label");
    assert_eq!(label_text(&label), "Equation (2.3), page 4\n\\label{eq:b}");
    // WP3.10: the popover is as tall as the wrapped text. A wrapping label that the scroller
    // measured for no width asked for blank lines under the text (about 30 px for two lines).
    assert!(block_on(testmode::wait_for_frames(&f.state.window, 1)));
    let scroller = label
        .ancestor(gtk::ScrolledWindow::static_type())
        .expect("the hover's scroller");
    let (_, natural, _, _) = label.measure(gtk::Orientation::Vertical, label.width());
    assert!(
        scroller.height() <= natural + 2,
        "the hover is {} px tall for {natural} px of text",
        scroller.height()
    );
    let (range, trigger) = hover::shown_range(view).unwrap();
    assert_eq!(&DOC[range.clone()], "eq:b");
    assert_eq!(trigger, Trigger::Pointer);
    // It points at the label.
    let (pointing, rect) = popover.pointing_to();
    assert!(pointing, "the popover points at the label");
    let (x, y) = point_of(&editor, range.start);
    assert!(
        f64::from(rect.x()) <= x && f64::from(rect.y()) <= y,
        "{rect:?}"
    );
    assert!(f64::from(rect.y() + rect.height()) >= y);
    // The view keeps the keyboard.
    common::pump_for(Duration::from_millis(50));
    assert!(view.has_focus(), "the hover took the focus");
    assert_eq!(
        gtk::prelude::GtkWindowExt::focus(&f.state.window).as_ref(),
        Some(view.upcast_ref::<gtk::Widget>())
    );
    if let Ok(dir) = std::env::var("LEMMA_TEST_SNAPSHOTS") {
        for (scheme, name) in [
            (adw::ColorScheme::ForceLight, "m3-hover-light.png"),
            (adw::ColorScheme::ForceDark, "m3-hover-dark.png"),
        ] {
            adw::StyleManager::default().set_color_scheme(scheme);
            hover::hide(view);
            assert!(hover::show_at(&editor, offset, Trigger::Pointer));
            assert!(block_on(testmode::wait_for_frames(&f.state.window, 2)));
            let shot = Path::new(&dir).join(name);
            block_on(testmode::snapshot_next_frame(&f.state.window, &shot)).unwrap();
            println!("  saved {}", shot.display());
        }
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::Default);
    }
    hover::hide(view);
    assert!(!popover.is_visible());
    assert!(!hover::is_shown(view));
    assert!(!hover::show_at(
        &editor,
        offset_of(&editor, "See", 0),
        Trigger::Pointer
    ));
}

fn hover_pointer() {
    let f = fixture("hover-pointer", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let view = editor.view();
    let calls = Rc::new(Cell::new(0));
    hover::register(
        view,
        hover::PRIORITY_REFS,
        label_provider("eq:b", "Equation (2.3)", calls.clone()),
    )
    .unwrap();
    let triggers: Rc<RefCell<Vec<Trigger>>> = Rc::default();
    let seen = triggers.clone();
    hover::register(view, 500, move |ctx| {
        seen.borrow_mut().push(ctx.trigger);
        None
    })
    .unwrap();
    let label = offset_of(&editor, "eq:b", 0);
    let (x, y) = point_of(&editor, label + 1);
    for offset in label..label + 4 {
        assert_character_columns(&editor, offset);
    }
    // Past the end of a line, and over the gutter: no character.
    let end = offset_of(&editor, "{knuth}.", 8);
    let end_rect = editor.view().iter_location(&editor.iter_near(end).unwrap());
    let (ex, ey) = editor.view().buffer_to_window_coords(
        gtk::TextWindowType::Widget,
        end_rect.x() + 40,
        end_rect.y() + 2,
    );
    assert_eq!(editor.offset_at_point(f64::from(ex), f64::from(ey)), None);
    assert_eq!(editor.offset_at_point(1.0, y), None, "the gutter");

    // Resting on the label shows it after the delay, not before.
    hover::pointer_moved(view, x, y);
    common::pump_for(Duration::from_millis(20));
    assert!(!hover::is_shown(view), "before the delay");
    common::pump_until(TIMEOUT, || hover::is_shown(view));
    assert_eq!(*triggers.borrow(), [Trigger::Pointer]);
    let popover = hover::popover(view).unwrap();
    common::pump_until(TIMEOUT, || popover.is_visible());
    assert_eq!(calls.get(), 1);

    // Moving inside the label keeps it, without asking again.
    let (x2, y2) = point_of(&editor, label + 3);
    hover::pointer_moved(view, x2, y2);
    common::pump_for(hover::LEAVE_GRACE + Duration::from_millis(150));
    assert!(popover.is_visible());
    assert_eq!(calls.get(), 1);

    // Onto the popover: it stays (grace) even though the pointer left the range.
    let (ox, oy) = point_of(&editor, offset_of(&editor, "See", 1));
    hover::pointer_moved(view, ox, oy);
    common::pump_for(hover::LEAVE_GRACE / 3);
    assert!(popover.is_visible(), "within the grace");
    // Leaving for plain text hides it.
    common::pump_until(TIMEOUT, || !popover.is_visible());
    assert!(!hover::is_shown(view));

    // Back on the label, then out of the view.
    hover::pointer_moved(view, x, y);
    common::pump_until(TIMEOUT, || popover.is_visible());
    hover::pointer_left(view);
    common::pump_until(TIMEOUT, || !popover.is_visible());

    // `[editor] hover = false`: no pointer hovers.
    let mut config = (*f.state.config.get()).clone();
    config.editor.hover = false;
    f.state.set_config(Rc::new(config));
    hover::pointer_moved(view, x2, y2);
    common::pump_for(Duration::from_millis(300));
    assert!(!hover::is_shown(view));
    // The keyboard still works.
    assert!(hover::show_at(&editor, label, Trigger::Keyboard));
}

fn hover_hiding() {
    let long: String = DOC.replace(
        "\\end{document}\n",
        &format!("{}\\end{{document}}\n", "filler line\n".repeat(200)),
    );
    let f = fixture("hover-hiding", &[("main.tex", &long)]);
    let editor = f.open("main.tex");
    let view = editor.view();
    let calls = Rc::new(Cell::new(0));
    hover::register(
        view,
        hover::PRIORITY_REFS,
        label_provider("eq:b", "Equation (2.3)", calls.clone()),
    )
    .unwrap();
    let label = offset_of(&editor, "eq:b", 1);
    let (x, y) = point_of(&editor, label);
    let shown = || hover::popover(view).is_some_and(|p| p.is_visible());

    // Typing (a key in the view) hides it, and it does not come back by itself.
    hover::pointer_moved(view, x, y);
    common::pump_until(TIMEOUT, &shown);
    let event = keys::event_for(&editor, gdk::Key::a, gdk::ModifierType::empty());
    keys::dispatch(view, &event);
    assert!(!hover::is_shown(view));
    common::pump_for(Duration::from_millis(250));
    assert!(!shown(), "the pointer has to move again");
    // A modifier alone does not hide it.
    hover::pointer_moved(view, x + 1.0, y);
    common::pump_until(TIMEOUT, &shown);
    keys::dispatch(
        view,
        &keys::event_for(&editor, gdk::Key::Control_L, gdk::ModifierType::empty()),
    );
    assert!(shown());

    // A change of the text hides it.
    editor.buffer().insert(&mut editor.buffer().end_iter(), "%");
    assert!(!hover::is_shown(view));
    hover::pointer_moved(view, x, y);
    common::pump_until(TIMEOUT, &shown);

    // Scrolling hides it.
    let adjustment = view.vadjustment().unwrap();
    common::pump_until(TIMEOUT, || {
        adjustment.upper() > adjustment.page_size() + 50.0
    });
    adjustment.set_value(adjustment.value() + 40.0);
    assert!(!hover::is_shown(view));
    common::pump_for(Duration::from_millis(250));
    assert!(!shown());

    // A place scrolled out of the view gets no popover (it would point outside the window).
    adjustment.set_value(adjustment.upper() - adjustment.page_size());
    common::pump_for(Duration::from_millis(50));
    let before = common::logged_problems();
    assert!(
        hover::show_at(&editor, label, Trigger::Pointer),
        "the provider answered"
    );
    common::pump_for(Duration::from_millis(50));
    assert!(!shown() && !hover::is_shown(view));
    common::assert_no_new_problems(before);
    // The keyboard brings the cursor into view first.
    set_cursor(&editor, label);
    assert!(hover::show_at_cursor(&f.state));
    common::pump_until(TIMEOUT, shown);
    common::assert_no_new_problems(before);
}

fn hover_pending() {
    let f = fixture("hover-pending", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let view = editor.view();
    type Senders = Rc<RefCell<Vec<async_channel::Sender<Option<HoverContent>>>>>;
    let senders: Senders = Rc::default();
    let keep = senders.clone();
    hover::register(view, hover::PRIORITY_MATH, move |ctx| {
        if !ctx.context.is_math() {
            return None;
        }
        // The whole formula.
        let start = ctx.line_start() + ctx.line_text.find('$')?;
        let end = ctx.line_start() + ctx.line_text.rfind('$')? + 1;
        ctx.set_range(start..end);
        let (tx, rx) = async_channel::bounded(1);
        keep.borrow_mut().push(tx);
        Some(HoverContent::pending(async move {
            rx.recv().await.ok().flatten()
        }))
    })
    .unwrap();
    let math = offset_of(&editor, "\\cF \\to", 0);
    assert!(hover::show_at(&editor, math, Trigger::Pointer));
    assert!(hover::is_shown(view));
    let (range, _) = hover::shown_range(view).unwrap();
    assert_eq!(&DOC[range], "$\\cF \\to \\Hom$");
    assert!(hover::popover(view).is_none_or(|p| !p.is_visible()));

    // A newer hover wins: the older answer is dropped.
    let to = offset_of(&editor, " \\to", 1);
    assert!(hover::show_at(&editor, to, Trigger::Pointer));
    assert_eq!(senders.borrow().len(), 2);
    let first = senders.borrow()[0].clone();
    let _ = first.try_send(Some(HoverContent::text("old")));
    common::pump_for(Duration::from_millis(50));
    assert!(hover::popover(view).is_none_or(|p| !p.is_visible()));

    // Slow: a spinner first, then the answer.
    common::pump_until(TIMEOUT, || {
        hover::shown_widget(view).is_some_and(|w| w.is::<adw::Spinner>())
    });
    let rendered = gtk::Label::new(Some("rendered formula"));
    senders.borrow()[1]
        .try_send(Some(HoverContent::Widget(rendered.clone().upcast())))
        .unwrap();
    common::pump_until(TIMEOUT, || {
        hover::shown_widget(view).as_ref() == Some(rendered.upcast_ref())
    });
    // Nothing after all: the popover goes.
    assert!(hover::show_at(&editor, math, Trigger::Pointer));
    let last = senders.borrow().last().unwrap().clone();
    last.try_send(None).unwrap();
    common::pump_until(TIMEOUT, || !hover::is_shown(view));
    common::pump_until(TIMEOUT, || {
        hover::popover(view).is_some_and(|p| !p.is_visible())
    });
}

fn hover_keyboard() {
    let f = fixture_with("hover-keyboard", &[("main.tex", DOC)], |c| {
        c.editor.vim = true
    });
    let editor = f.open("main.tex");
    let view = editor.view();
    let triggers: Rc<RefCell<Vec<(Trigger, usize)>>> = Rc::default();
    let seen = triggers.clone();
    hover::register(view, hover::PRIORITY_REFS, move |ctx| {
        let (range, name) = ctx.item()?;
        seen.borrow_mut().push((ctx.trigger, range.start));
        Some(HoverContent::text(name))
    })
    .unwrap();
    view.grab_focus();
    let label = offset_of(&editor, "eq:a", 0);

    // Alt+K (win.show-hover) at the cursor.
    set_cursor(&editor, label + 2);
    f.activate("win.show-hover");
    assert_eq!(*triggers.borrow(), [(Trigger::Keyboard, label)]);
    assert_eq!(hover::shown_range(view).unwrap().1, Trigger::Keyboard);
    let popover = hover::popover(view).unwrap();
    common::pump_until(TIMEOUT, || popover.is_visible());
    // The pointer moving elsewhere leaves a keyboard hover alone.
    let (x, y) = point_of(&editor, offset_of(&editor, "See", 0));
    hover::pointer_moved(view, x, y);
    common::pump_for(hover::LEAVE_GRACE + Duration::from_millis(100));
    assert!(popover.is_visible());

    // Right after a name (insert mode, plain editing): the character before the cursor.
    hover::hide(view);
    triggers.borrow_mut().clear();
    set_cursor(&editor, label + "eq:a".len());
    assert!(hover::show_at_cursor(&f.state));
    assert_eq!(triggers.borrow().last(), Some(&(Trigger::Keyboard, label)));
    // Nothing there: false.
    hover::hide(view);
    set_cursor(&editor, offset_of(&editor, "See", 1));
    assert!(!hover::show_at_cursor(&f.state));
    assert!(!hover::is_shown(view));

    // `K` in Vim normal mode (a [vim.normal] binding of win.show-hover).
    assert_eq!(editor.vim_status().unwrap().mode, VimMode::Normal);
    set_cursor(&editor, label + 1);
    triggers.borrow_mut().clear();
    let k = keys::event_for(&editor, gdk::Key::K, gdk::ModifierType::SHIFT_MASK);
    assert_eq!(keys::dispatch(view, &k), glib::Propagation::Stop);
    common::pump_until(TIMEOUT, || hover::is_shown(view));
    assert_eq!(*triggers.borrow(), [(Trigger::Keyboard, label)]);
    assert_eq!(editor.document().text(), DOC, "K changed nothing");
    // Any other key hides it and goes on to Vim.
    let j = keys::event_for(&editor, gdk::Key::j, gdk::ModifierType::empty());
    assert_eq!(keys::dispatch(view, &j), glib::Propagation::Proceed);
    assert!(!hover::is_shown(view));
}

fn hover_invalid_markup() {
    let f = fixture("hover-invalid", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    let view = editor.view();
    hover::register(view, 1, |_| {
        Some(HoverContent::Markup("a < b & <i>c".into()))
    })
    .unwrap();
    let before = common::logged_problems();
    assert!(hover::show_at(&editor, 0, Trigger::Keyboard));
    common::pump_for(Duration::from_millis(50));
    let label = hover::shown_widget(view).unwrap();
    assert_eq!(label_text(&label), "a < b & <i>c");
    common::assert_no_new_problems(before);
}

fn registries_detach() {
    let f = fixture("detach", &[("main.tex", DOC), ("b.tex", "B\n")]);
    let editor = f.open("main.tex");
    f.open("b.tex");
    let view = editor.view().clone();
    hover::register(&view, 1, |_| Some(HoverContent::text("x"))).unwrap();
    goto::register(&view, 1, |_| None).unwrap();
    assert!(hover::show_at(&editor, 0, Trigger::Keyboard));
    let popover = hover::popover(&view).unwrap();
    assert!(popover.parent().is_some());
    let weak_view = view.downgrade();
    drop(view);
    let page = f.state.tab_view.page(editor.widget());
    drop(editor);
    f.state.tab_view.close_page(&page);
    common::pump_for(Duration::from_millis(50));
    assert!(popover.parent().is_none(), "the popover left the view");
    assert!(!popover.is_visible());
    // The view went with its tab (nothing of the registries kept it).
    common::pump_until(TIMEOUT, || weak_view.upgrade().is_none());
}

// ----- goto -----

fn goto_files() -> Vec<(&'static str, &'static str)> {
    vec![("main.tex", DOC), ("chapters/a.tex", CHAPTER)]
}

/// The resolvers WP3.2 will have, in miniature: `\input{chapters/a}` → the file, `eq:b` → the
/// chapter's label line.
fn register_test_resolvers(f: &Fixture, editor: &Editor) {
    let (file, chapter) = (f.path("chapters/a.tex"), f.path("chapters/a.tex"));
    goto::register(editor.view(), goto::PRIORITY_REFS, move |ctx| {
        let (_, name) = ctx.item()?;
        match name {
            "chapters/a" => Some(GotoTarget::File {
                path: file.clone(),
                offset: 0,
            }),
            "eq:b" => Some(GotoTarget::Line {
                path: chapter.clone(),
                line: 1,
                find: Some("\\label{sec:a}".into()),
            }),
            _ => None,
        }
    })
    .unwrap();
}

fn goto_priority() {
    let f = fixture("goto-priority", &goto_files());
    let editor = f.open("main.tex");
    let view = editor.view();
    assert_eq!(goto::resolver_count(view), 0, "WP3.2's resolvers are stubs");
    let order: Rc<RefCell<Vec<&str>>> = Rc::default();
    let log = |name: &'static str, offset: Option<usize>| {
        let order = order.clone();
        let path = f.path("main.tex");
        move |_: &goto::GotoCtx| {
            order.borrow_mut().push(name);
            offset.map(|offset| GotoTarget::File {
                path: path.clone(),
                offset,
            })
        }
    };
    goto::register(view, 1, log("low", Some(1))).unwrap();
    let high = goto::register(view, 9, log("high", None)).unwrap();
    goto::register(view, 5, log("mid", Some(5))).unwrap();
    let target = goto::resolve_at(&editor, 0, Trigger::Keyboard).unwrap();
    assert_eq!(
        target,
        GotoTarget::File {
            path: f.path("main.tex"),
            offset: 5
        }
    );
    assert_eq!(*order.borrow(), ["high", "mid"]);
    goto::unregister(view, high);
    assert_eq!(goto::resolver_count(view), 2);
}

fn wait_cursor(f: &Fixture, file: &str, offset: usize) {
    let path = f.path(file);
    common::pump_until(TIMEOUT, || {
        f.active_path().as_deref() == Some(path.as_path())
            && f.state.active_editor().unwrap().cursor_offset() == offset
    });
}

fn goto_jumps() {
    let f = fixture("goto-jumps", &goto_files());
    let editor = f.open("main.tex");
    register_test_resolvers(&f, &editor);
    let start = offset_of(&editor, "eq:b", 2);
    set_cursor(&editor, start);
    assert!(goto::goto_definition(&f.state));
    // The chapter, on its label.
    let label = CHAPTER.find("\\label{sec:a}").unwrap();
    wait_cursor(&f, "chapters/a.tex", label);
    let chapter = f.state.active_editor().unwrap();
    register_test_resolvers(&f, &chapter);
    assert_eq!(
        goto::jumps(&f.state),
        (vec![(f.path("main.tex"), start)], vec![])
    );
    assert!(chapter.view().has_focus());
    // Back to where the cursor was, then forward again.
    assert!(goto::back(&f.state));
    wait_cursor(&f, "main.tex", start);
    assert_eq!(
        goto::jumps(&f.state),
        (vec![], vec![(f.path("chapters/a.tex"), label)])
    );
    assert!(goto::can_go_forward(&f.state) && !goto::can_go_back(&f.state));
    f.activate("win.goto-forward");
    wait_cursor(&f, "chapters/a.tex", label);
    f.activate("win.goto-back");
    wait_cursor(&f, "main.tex", start);
    // Nowhere further back.
    assert!(!goto::back(&f.state));
    // A new jump drops the forward list; `\input` goes to the file's start.
    set_cursor(&editor, offset_of(&editor, "chapters/a", 1));
    f.activate("win.goto-definition");
    wait_cursor(&f, "chapters/a.tex", 0);
    let (back, forward) = goto::jumps(&f.state);
    assert_eq!(back.len(), 1);
    assert!(forward.is_empty());
    // Nothing at the cursor: false (a toast), nothing recorded.
    f.state.select_editor(&editor);
    set_cursor(&editor, offset_of(&editor, "See", 0));
    assert!(!goto::goto_definition(&f.state));
    assert_eq!(goto::jumps(&f.state).0.len(), 1);
}

fn goto_marks_and_cap() {
    let f = fixture("goto-marks", &goto_files());
    let editor = f.open("main.tex");
    register_test_resolvers(&f, &editor);
    let start = offset_of(&editor, "eq:b", 0);
    set_cursor(&editor, start);
    assert!(goto::goto_definition(&f.state));
    wait_cursor(&f, "chapters/a.tex", CHAPTER.find("\\label").unwrap());
    // An edit before the recorded place moves it.
    let buffer = editor.buffer();
    buffer.insert(&mut buffer.start_iter(), "% new first line\n");
    assert_eq!(
        goto::jumps(&f.state).0,
        vec![(f.path("main.tex"), start + 17)]
    );
    assert!(goto::back(&f.state));
    wait_cursor(&f, "main.tex", start + 17);

    // At most MAX_JUMPS entries: the oldest go.
    let text_len = editor.document().text().len();
    for i in 0..goto::MAX_JUMPS + 5 {
        set_cursor(&editor, i % text_len);
        let target = GotoTarget::File {
            path: f.path("main.tex"),
            offset: (i + 7) % text_len,
        };
        block_on(goto::navigate(&f.state, target)).unwrap();
    }
    assert_eq!(goto::jumps(&f.state).0.len(), goto::MAX_JUMPS);
    // A jump to where the cursor already is records nothing.
    let here = editor.cursor_offset();
    let before = goto::jumps(&f.state).0.len();
    block_on(goto::navigate(
        &f.state,
        GotoTarget::File {
            path: f.path("main.tex"),
            offset: here,
        },
    ))
    .unwrap();
    assert_eq!(goto::jumps(&f.state).0.len(), before);
}

fn goto_vim() {
    let f = fixture_with("goto-vim", &goto_files(), |c| c.editor.vim = true);
    let editor = f.open("main.tex");
    register_test_resolvers(&f, &editor);
    let view = editor.view();
    view.grab_focus();
    assert_eq!(editor.vim_status().unwrap().mode, VimMode::Normal);
    let key = |editor: &Editor, keyval: gdk::Key, state: gdk::ModifierType| {
        keys::dispatch(editor.view(), &keys::event_for(editor, keyval, state))
    };
    let none = gdk::ModifierType::empty();

    // Ctrl+O with an empty jump list is Vim's.
    assert_eq!(
        key(&editor, gdk::Key::o, gdk::ModifierType::CONTROL_MASK),
        glib::Propagation::Proceed
    );

    // `gd`: g is held (it starts bindings), d completes gd.
    let start = offset_of(&editor, "eq:b", 1);
    set_cursor(&editor, start);
    assert_eq!(key(&editor, gdk::Key::g, none), glib::Propagation::Stop);
    assert_eq!(key(&editor, gdk::Key::d, none), glib::Propagation::Stop);
    let label = CHAPTER.find("\\label{sec:a}").unwrap();
    wait_cursor(&f, "chapters/a.tex", label);
    assert_eq!(editor.document().text(), DOC, "gd typed nothing");

    // Ctrl+O in normal mode goes back now.
    let chapter = f.state.active_editor().unwrap();
    assert_eq!(chapter.vim_status().unwrap().mode, VimMode::Normal);
    assert_eq!(
        key(&chapter, gdk::Key::o, gdk::ModifierType::CONTROL_MASK),
        glib::Propagation::Stop
    );
    wait_cursor(&f, "main.tex", start);
    // Not in insert mode (Vim's Ctrl+O there), nor with something pending.
    block_on(goto::navigate(
        &f.state,
        GotoTarget::File {
            path: f.path("main.tex"),
            offset: 0,
        },
    ))
    .unwrap();
    assert!(goto::can_go_back(&f.state));
    let im = editor.vim().unwrap();
    let feed = |keyval: gdk::Key| {
        assert_eq!(key(&editor, keyval, none), glib::Propagation::Proceed);
        lemma_app::editor::vim::feed_key(&im, editor.view(), keyval, none);
    };
    feed(gdk::Key::_2);
    common::pump_until(TIMEOUT, || {
        editor.vim_status().is_some_and(|s| s.command == "2")
    });
    assert_eq!(
        key(&editor, gdk::Key::o, gdk::ModifierType::CONTROL_MASK),
        glib::Propagation::Proceed,
        "2<C-o> is Vim's"
    );
    lemma_app::editor::vim::cancel_pending(&im, editor.view());
    feed(gdk::Key::i);
    common::pump_until(TIMEOUT, || {
        editor
            .vim_status()
            .is_some_and(|s| s.mode == VimMode::Insert)
    });
    assert_eq!(
        key(&editor, gdk::Key::o, gdk::ModifierType::CONTROL_MASK),
        glib::Propagation::Proceed
    );
}

fn goto_pointer() {
    let f = fixture("goto-pointer", &goto_files());
    let editor = f.open("main.tex");
    register_test_resolvers(&f, &editor);
    let file = offset_of(&editor, "chapters/a", 0);
    let (x, y) = point_of(&editor, file + 3);

    // Ctrl held over a name with a target: underlined, a hand.
    goto::pointer_moved(&editor, x, y, true);
    let link = goto::link_range(editor.view()).expect("a link");
    assert_eq!(&DOC[link.clone()], "chapters/a");
    let tag = editor
        .buffer()
        .tag_table()
        .lookup(goto::LINK_TAG)
        .expect("the link tag");
    let iter = editor.iter_near(file + 1).unwrap();
    assert!(iter.has_tag(&tag));
    let cursor = |editor: &Editor| {
        editor
            .view()
            .cursor()
            .and_then(|c| c.name())
            .map(|n| n.to_string())
    };
    assert_eq!(cursor(&editor).as_deref(), Some("pointer"));
    // Without Ctrl, or over plain text, it goes.
    goto::pointer_moved(&editor, x, y, false);
    assert_eq!(goto::link_range(editor.view()), None);
    assert!(!editor.iter_near(file + 1).unwrap().has_tag(&tag));
    let (px, py) = point_of(&editor, offset_of(&editor, "See", 1));
    goto::pointer_moved(&editor, px, py, true);
    assert_eq!(goto::link_range(editor.view()), None);
    assert_eq!(cursor(&editor).as_deref(), Some("text"));
    assert!(!editor.document().is_dirty(), "underlining is not an edit");

    // Ctrl+click.
    assert!(!goto::ctrl_click(
        &f.state,
        &editor,
        offset_of(&editor, "See", 0)
    ));
    assert!(goto::ctrl_click(&f.state, &editor, file + 2));
    wait_cursor(&f, "chapters/a.tex", 0);
}

fn goto_targets() {
    let f = fixture(
        "goto-targets",
        &[
            ("main.tex", DOC),
            ("chapters/a.tex", CHAPTER),
            ("fig.png", "not really a png"),
        ],
    );
    f.open("main.tex");
    // A line: the first non-blank character, or the text to find; past the end: the last line.
    block_on(goto::navigate(
        &f.state,
        GotoTarget::Line {
            path: f.path("chapters/a.tex"),
            line: 2,
            find: None,
        },
    ))
    .unwrap();
    wait_cursor(&f, "chapters/a.tex", CHAPTER.find("Text").unwrap());
    block_on(goto::navigate(
        &f.state,
        GotoTarget::Line {
            path: f.path("chapters/a.tex"),
            line: 0,
            find: Some("{A}".into()),
        },
    ))
    .unwrap();
    wait_cursor(&f, "chapters/a.tex", "\\section".len());
    block_on(goto::navigate(
        &f.state,
        GotoTarget::File {
            path: f.path("chapters/a.tex"),
            offset: 10_000,
        },
    ))
    .unwrap();
    wait_cursor(&f, "chapters/a.tex", CHAPTER.len());

    // The PDF: an error until there is one.
    let e = block_on(goto::navigate(
        &f.state,
        GotoTarget::Pdf {
            page: 0,
            rect: (72.0, 100.0, 50.0, 12.0),
        },
    ))
    .unwrap_err();
    assert!(e.contains("build the document first"), "{e}");
    let pdf_file = f.dir.join("main.pdf");
    std::fs::copy(common::fixtures().join("proj/main.pdf"), &pdf_file).unwrap();
    let pdf = f.state.pdf.get().unwrap();
    pdf.open(&pdf_file);
    common::pump_until(TIMEOUT, || {
        pdf.pdf().is_some() && pdf.view().page_count() > 0
    });
    f.state.pdf_pane.set_visible(false);
    block_on(goto::navigate(
        &f.state,
        GotoTarget::Pdf {
            page: 1,
            rect: (72.0, 100.0, 50.0, 12.0),
        },
    ))
    .unwrap();
    assert!(f.state.pdf_pane.is_visible());
    assert_eq!(
        pdf.view().highlight().map(|(page, _)| page),
        Some(1),
        "the place is flashed"
    );

    // An image: the launcher (here the test hook), never an editor tab.
    let launched: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let log = launched.clone();
    goto::set_launch_hook(
        &f.state,
        Some(Box::new(move |path: &Path| {
            log.borrow_mut().push(path.to_path_buf());
            true
        })),
    );
    let tabs = f.state.editors().len();
    block_on(goto::navigate(
        &f.state,
        GotoTarget::External {
            path: f.path("fig.png"),
        },
    ))
    .unwrap();
    assert_eq!(*launched.borrow(), [f.path("fig.png")]);
    assert_eq!(f.state.editors().len(), tabs);
    // Without the hook a scripted run refuses to start other applications.
    goto::set_launch_hook(&f.state, None);
    let e = block_on(goto::navigate(
        &f.state,
        GotoTarget::External {
            path: f.path("fig.png"),
        },
    ))
    .unwrap_err();
    assert!(e.contains("scripted"), "{e}");
}

fn goto_back_handlers() {
    let f = fixture("goto-back-handlers", &goto_files());
    let editor = f.open("main.tex");
    register_test_resolvers(&f, &editor);
    set_cursor(&editor, offset_of(&editor, "eq:b", 0));
    assert!(goto::goto_definition(&f.state));
    wait_cursor(&f, "chapters/a.tex", CHAPTER.find("\\label").unwrap());
    let calls = Rc::new(Cell::new(0));
    let handled = Rc::new(Cell::new(true));
    let (c, h) = (calls.clone(), handled.clone());
    let id = goto::add_back_handler(&f.state, move |_| {
        c.set(c.get() + 1);
        h.get()
    })
    .unwrap();
    // The handler went back itself: the jump list stays.
    f.activate("win.goto-back");
    assert_eq!(calls.get(), 1);
    assert_eq!(goto::jumps(&f.state).0.len(), 1);
    // It declines: the jump list goes back.
    handled.set(false);
    f.activate("win.goto-back");
    assert_eq!(calls.get(), 2);
    assert_eq!(goto::jumps(&f.state).0.len(), 0);
    goto::remove_back_handler(&f.state, id);
    f.activate("win.goto-back");
    assert_eq!(calls.get(), 2);
}

// ----- session, configuration, stubs -----

fn session_hooks() {
    let f = fixture("session", &[("main.tex", DOC), ("b.tex", "B\n")]);
    // A window that quits by itself (tests, snapshots) saves no session by default.
    assert!(!f.state.session_saves_enabled());
    f.state.session_changed();
    assert!(!f.state.session_save_pending());

    f.state.set_session_autosave(Some(true));
    f.state.set_session_debounce(Duration::from_millis(250));
    assert!(f.state.session_saves_enabled());
    let editor = block_on(f.state.open_file(&f.dir.join("main.tex"))).unwrap();
    assert!(f.state.session_save_pending(), "a tab opened");
    common::pump_until(TIMEOUT, || !f.state.session_save_pending());
    // Debounced: a burst of changes keeps postponing one save.
    for i in 0..6 {
        set_cursor(&editor, i * 3);
        assert!(f.state.session_save_pending(), "cursor move {i}");
        common::pump_for(Duration::from_millis(60));
        assert!(
            f.state.session_save_pending(),
            "still waiting after move {i}"
        );
    }
    common::pump_until(TIMEOUT, || !f.state.session_save_pending());
    let expect_save = |what: &str, change: &dyn Fn()| {
        common::pump_until(TIMEOUT, || !f.state.session_save_pending());
        change();
        assert!(f.state.session_save_pending(), "{what}");
    };
    expect_save("the sidebar page", &|| {
        f.state.sidebar.show_page(PAGE_SEARCH)
    });
    expect_save("the PDF pane", &|| {
        f.state.pdf_pane.set_visible(!f.state.pdf_pane.is_visible());
    });
    expect_save("the PDF strip", &|| {
        let split = &f.state.pdf_sidebar.split;
        split.set_show_sidebar(!split.shows_sidebar());
    });
    expect_save("a new tab", &|| {
        block_on(f.state.open_file(&f.dir.join("b.tex"))).unwrap();
    });
    expect_save("another tab selected", &|| f.state.select_editor(&editor));
    expect_save("the split", &|| {
        f.state.paned.set_position(f.state.paned.position() - 20);
    });
    // Closing saves at once.
    f.state.save_session_now();
    assert!(!f.state.session_save_pending());
    // `[session] restore = false`: nothing is saved.
    let mut config = (*f.state.config.get()).clone();
    config.session.restore = false;
    f.state.set_config(Rc::new(config));
    assert!(!f.state.session_saves_enabled());
    set_cursor(&editor, 1);
    assert!(!f.state.session_save_pending());
}

fn edit_config() {
    let f = fixture("edit-config", &[]);
    assert!(
        f.state.edit_config_file(|t| Ok(t.to_owned())).is_err(),
        "no file"
    );
    let dotfiles = f.dir.join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("lemma.toml");
    let original = "# my settings\n[editor]\nwrap = false # long lines\n";
    std::fs::write(&target, original).unwrap();
    let config = f.dir.join("config/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &config).unwrap();
    f.state
        .set_settings_files(Some(SettingsFiles::next_to(config.clone())));
    assert_eq!(f.state.config_file(), Some(config.clone()));

    // Written where the link points, comments kept, applied at once.
    let configs: Rc<Cell<u32>> = Rc::default();
    let count = configs.clone();
    f.state
        .connect_config_changed(move |_, _| count.set(count.get() + 1));
    f.state
        .edit_config_file(|text| {
            lemma_config::set_value(text, "pdf", "thumbnails", ConfigValue::Bool(true))
        })
        .unwrap();
    assert!(std::fs::symlink_metadata(&config).unwrap().is_symlink());
    let written = std::fs::read_to_string(&target).unwrap();
    assert!(written.starts_with(original), "{written}");
    assert!(written.contains("[pdf]\nthumbnails = true\n"), "{written}");
    assert!(f.state.config.pdf.thumbnails);
    assert!(!f.state.config.editor.wrap);
    assert!(f.state.pdf_sidebar.split.shows_sidebar(), "applied live");
    assert_eq!(configs.get(), 1);
    // Back to the default.
    f.state
        .edit_config_file(|text| lemma_config::remove_key(text, "editor", "wrap"))
        .unwrap();
    assert!(f.state.config.editor.wrap);
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .starts_with("# my settings\n[editor]\n")
    );

    // An invalid result is refused and nothing is written.
    let before = std::fs::read_to_string(&target).unwrap();
    let e = f
        .state
        .edit_config_file(|text| Ok(format!("{text}[pdf2]\nx = 1\n")))
        .unwrap_err();
    assert!(e.contains("pdf2"), "{e}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
    let e = f
        .state
        .edit_config_file(|_| Err("refused".into()))
        .unwrap_err();
    assert_eq!(e, "refused");

    // A missing file is created; /dev/null is never written.
    let fresh = f.dir.join("fresh/config.toml");
    f.state
        .set_settings_files(Some(SettingsFiles::next_to(fresh.clone())));
    f.state
        .edit_config_file(|t| {
            lemma_config::set_value(t, "outline", "follow_cursor", ConfigValue::Bool(false))
        })
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&fresh).unwrap(),
        "[outline]\nfollow_cursor = false\n"
    );
    assert!(!f.state.config.outline.follow_cursor);
    f.state
        .set_settings_files(Some(SettingsFiles::next_to("/dev/null".into())));
    let e = f
        .state
        .edit_config_file(|t| lemma_config::set_value(t, "editor", "vim", ConfigValue::Bool(false)))
        .unwrap_err();
    assert!(e.contains("not a file"), "{e}");
}

fn stubs_wired() {
    let f = fixture("stubs", &[("main.tex", DOC)]);
    let editor = f.open("main.tex");
    editor.view().grab_focus();
    // Every M3 action runs (the work packages filled the stubs: dialogs, popovers, the Search
    // page, toasts where nothing is under the cursor) and none changes the text.
    for action in [
        "win.command-palette",
        "win.quick-open",
        "win.shortcuts",
        "win.preferences",
        "win.find-references",
        "win.rename-label",
        "win.jump-pair",
        "win.history",
        "win.word-count",
        "win.goto-definition",
        "win.goto-back",
        "win.goto-forward",
        "win.show-hover",
        "win.project-search",
    ] {
        f.activate(action);
    }
    common::pump_for(Duration::from_millis(30));
    assert_eq!(editor.document().text(), DOC);
    // Detaching the PDF (WP3.4) moves it into a window of its own; again brings it back.
    let detached = || {
        let state = f.state.window.lookup_action("pdf-detach").unwrap().state();
        state.and_then(|s| s.get::<bool>())
    };
    f.activate("win.pdf-detach");
    assert_eq!(detached(), Some(true));
    assert!(lemma_app::pdf::detach::is_detached(&f.state));
    f.activate("win.pdf-detach");
    assert_eq!(detached(), Some(false));
    assert!(!lemma_app::pdf::detach::is_detached(&f.state));
    // The session stubs: restore resolves.
    block_on(lemma_app::session::restore(&f.state)).unwrap();
    lemma_app::session::save(&f.state);
}

// ----- real key events -----

mod real_keys {
    //! The accelerators and keys of M3 on real key events (a private Broadway daemon, see
    //! `tests/broadway_input`): they keep clear of Vim and the M2 key layer.

    use super::*;
    use broadway_input::{ALT, Browser, CONTROL, SHIFT};

    /// Prints a progress line with the time (the child's stdout is shown when it fails).
    macro_rules! step {
        ($($arg:tt)*) => {
            println!(
                "child {}: {}",
                glib::DateTime::now_local()
                    .and_then(|t| t.format("%H:%M:%S.%f"))
                    .map(|t| t.to_string())
                    .unwrap_or_default(),
                format!($($arg)*)
            )
        };
    }

    pub fn parent() {
        broadway_input::run_child("m3_real_keys_child", 31);
    }

    /// The window actions activated, in order.
    type Activated = Rc<RefCell<Vec<String>>>;

    pub fn child() {
        let long = DOC.replace(
            "\\end{document}\n",
            &format!("{}\\end{{document}}\n", "filler line\n".repeat(150)),
        );
        let f = fixture_with(
            "real-keys",
            &[("main.tex", long.as_str()), ("chapters/a.tex", CHAPTER)],
            |c| c.editor.vim = true,
        );
        step!("fixture");
        let editor = f.open("main.tex");
        register_test_resolvers(&f, &editor);
        app::set_accels(&f.state.app, &f.state.config.keys);
        let activated: Activated = Rc::default();
        for name in [
            "command-palette",
            "quick-open",
            "project-search",
            "shortcuts",
            "preferences",
            "pdf-search",
            "goto-definition",
            "find-references",
            "rename-label",
            "jump-pair",
            "goto-back",
            "goto-forward",
            "show-hover",
            "agent-inline-edit",
            "problem-next",
            "problem-prev",
        ] {
            let log = activated.clone();
            let action = f.state.window.lookup_action(name).unwrap();
            let action = action.downcast::<gio::SimpleAction>().unwrap();
            action.connect_activate(move |_, _| log.borrow_mut().push(name.to_owned()));
        }
        // Which keys reach the editor's key layer (and so Vim).
        let reached: Rc<RefCell<Vec<(gdk::Key, gdk::ModifierType)>>> = Rc::default();
        let log = reached.clone();
        keys::register(editor.view(), i32::MAX, move |event: &KeyEvent, _| {
            log.borrow_mut().push((event.keyval, event.state));
            glib::Propagation::Proceed
        })
        .unwrap();
        let view = editor.view().clone();
        let focus_view = || {
            f.state.sidebar.split.set_show_sidebar(false);
            view.grab_focus();
            common::pump_until(TIMEOUT, || view.has_focus());
        };
        step!("window up");
        common::pump_until(TIMEOUT, || f.state.window.is_active());
        step!("window active");
        focus_view();
        step!("view focused");
        // Frames at full speed: the PDF view needs layout.
        let mut browser = Browser::connect_drawing();
        browser.settle = Duration::from_millis(30);
        let take = |log: &Activated| std::mem::take(&mut *log.borrow_mut());

        // Accelerators in Vim normal mode: each runs its action, Vim never sees the key.
        for (key, mods, action) in [
            (gdk::Key::p, CONTROL, "quick-open"),
            (gdk::Key::P, CONTROL | SHIFT, "command-palette"),
            // Broadway's keymap consumes no modifier: a keyboard that types `?` and `%` with
            // Shift gives these without it, as GDK does on Wayland.
            (gdk::Key::question, CONTROL, "shortcuts"),
            (gdk::Key::comma, CONTROL, "preferences"),
            (gdk::Key::F12, 0, "goto-definition"),
            (gdk::Key::F12, SHIFT, "find-references"),
            (gdk::Key::F2, 0, "rename-label"),
            (gdk::Key::F4, 0, "problem-next"),
            (gdk::Key::F4, SHIFT, "problem-prev"),
            (gdk::Key::percent, CONTROL, "jump-pair"),
            (gdk::Key::Right, ALT, "goto-forward"),
            (gdk::Key::Left, ALT, "goto-back"),
            (gdk::Key::k, ALT, "show-hover"),
            (gdk::Key::k, CONTROL, "agent-inline-edit"),
            (gdk::Key::F, CONTROL | SHIFT, "project-search"),
        ] {
            focus_view();
            hover::hide(&view);
            reached.borrow_mut().clear();
            step!("{key:?} {mods} for {action}");
            browser.key(key, mods);
            common::pump_until(TIMEOUT, || !activated.borrow().is_empty());
            assert_eq!(take(&activated), [action], "{key:?} {mods}");
            assert!(
                reached.borrow().iter().all(|(k, _)| *k != key),
                "{action}'s key reached the editor"
            );
            assert_eq!(
                editor.vim_status().unwrap().mode,
                VimMode::Normal,
                "{action}"
            );
            if action == "agent-inline-edit" {
                lemma_app::agent::inline::cancel(&f.state);
            }
            // The palette, quick open, the shortcuts window and Preferences are dialogs (WP3.5).
            if let Some(dialog) = f.state.window.visible_dialog() {
                dialog.force_close();
            }
        }
        assert_eq!(editor.document().text(), long, "no key was typed");

        step!("Ctrl+F in the editor");
        // Ctrl+F in the editor is Vim's (page down): win.pdf-search is not run.
        focus_view();
        reached.borrow_mut().clear();
        browser.key(gdk::Key::f, CONTROL);
        common::pump_until(TIMEOUT, || {
            reached
                .borrow()
                .iter()
                .any(|(k, s)| *k == gdk::Key::f && s.contains(gdk::ModifierType::CONTROL_MASK))
        });
        common::pump_for(Duration::from_millis(50));
        assert!(take(&activated).is_empty(), "Ctrl+F ran an action");

        step!("K, gd, Ctrl+O");
        // `K` and `gd` in normal mode: the [vim.normal] bindings.
        let label = offset_of(&editor, "eq:b", 1);
        set_cursor(&editor, label);
        hover::register(&view, 1, |ctx| Some(HoverContent::text(ctx.item()?.1))).unwrap();
        browser.key(gdk::Key::K, SHIFT);
        common::pump_until(TIMEOUT, || !activated.borrow().is_empty());
        assert_eq!(take(&activated), ["show-hover"]);
        common::pump_until(TIMEOUT, || hover::is_shown(&view));
        browser.text("gd");
        common::pump_until(TIMEOUT, || !activated.borrow().is_empty());
        assert_eq!(take(&activated), ["goto-definition"]);
        let label_line = CHAPTER.find("\\label{sec:a}").unwrap();
        wait_cursor(&f, "chapters/a.tex", label_line);
        // Ctrl+O there goes back (the jump list has an entry).
        let chapter = f.state.active_editor().unwrap();
        common::pump_until(TIMEOUT, || chapter.view().has_focus());
        browser.key(gdk::Key::o, CONTROL);
        wait_cursor(&f, "main.tex", label);
        assert_eq!(editor.document().text(), long);

        step!("Ctrl+F in the PDF");
        // With the PDF focused, Ctrl+F searches it.
        let pdf_file = f.dir.join("main.pdf");
        std::fs::copy(common::fixtures().join("proj/main.pdf"), &pdf_file).unwrap();
        let pdf = f.state.pdf.get().unwrap();
        f.state.pdf_pane.set_visible(true);
        pdf.open(&pdf_file);
        common::pump_until(TIMEOUT, || {
            pdf.pdf().is_some() && pdf.view().is_mapped() && pdf.view().height() > 0
        });
        pdf.view().grab_focus();
        common::pump_until(TIMEOUT, || pdf.view().has_focus());
        browser.key(gdk::Key::f, CONTROL);
        common::pump_until(TIMEOUT, || !activated.borrow().is_empty());
        assert_eq!(take(&activated), ["pdf-search"]);
    }
}
