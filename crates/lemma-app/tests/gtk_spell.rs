//! Spell checking (WP2.9b acceptance; ARCHITECTURE §5 "Spell check").
//!
//! libspelling checks the buffer in the background, so every test waits until the underlined
//! words are the expected ones (and checks again a little later that nothing else appears).
//! The cursor stays on the empty last line: libspelling never underlines the word at the
//! cursor (the word being typed).
//!
//! The words a word list adds stay accepted for the rest of the process (enchant's session list
//! of the language), so each test uses words of its own.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_spell`.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::editor::{Editor, highlight, spell};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    common::run(&[
        (
            "misspellings_in_text_are_flagged_and_latex_is_not",
            text_and_latex,
        ),
        ("words_of_the_users_lists_are_not_flagged", word_lists),
        ("context_menu_suggestions_and_actions", context_menu),
        ("the_toggle_and_config_reloads", toggle_and_reload),
        ("a_missing_dictionary_falls_back", missing_language),
        ("loaded_after_the_first_frame_only", lazy),
        ("bib_files_are_not_checked", bib),
        (
            "latex_on_lines_never_on_screen_is_not_flagged",
            offscreen_latex,
        ),
        ("detach_removes_everything", detach),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(15);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

// ───────────── Fixture ─────────────

struct Fixture {
    state: Rc<WindowState>,
    editor: Rc<Editor>,
    dir: PathBuf,
}

fn config(lists: Vec<PathBuf>) -> Config {
    let mut config = Config::default();
    config.build.on_save = false;
    config.build.on_agent_turn_end = false;
    config.editor.vim = false;
    config.spell.enabled = true;
    config.spell.language = "en_US".into();
    config.spell.word_lists = lists;
    config.agent.node = "/nonexistent/lemma-test/node".into();
    config.agent.claude = "/nonexistent/lemma-test/claude".into();
    config
}

/// A window on `file` (written with `text`) in a fresh project, after its first frame.
fn fixture(name: &str, file: &str, text: &str, mut config: Config) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    std::fs::write(dir.join(file), text).unwrap();
    config.agent.adapter_dir = dir.join(".no-adapter");
    let app = common::application();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, true);
    state.window.set_default_size(900, 700);
    state.pdf_pane.set_visible(false);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    let editor = block_on(state.open_file(&dir.join(file))).expect("open");
    common::pump_until(TIMEOUT, || editor.view().has_focus());
    let buffer = editor.buffer();
    buffer.place_cursor(&buffer.end_iter());
    Fixture { state, editor, dir }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

impl Fixture {
    /// Waits until the underlined words, sorted, are `expected`, and still are a little later.
    fn expect_flagged(&self, expected: &[&str], what: &str) {
        let mut expected: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        expected.sort();
        let flagged = || {
            let mut words = spell::misspelled(&self.editor);
            words.sort();
            words
        };
        let deadline = Instant::now() + TIMEOUT;
        while !(spell::is_checking(&self.editor) && flagged() == expected) {
            assert!(
                Instant::now() < deadline,
                "{what}: flagged {:?}, expected {expected:?}",
                flagged()
            );
            common::pump_for(Duration::from_millis(20));
        }
        common::pump_for(Duration::from_millis(300));
        assert_eq!(flagged(), expected, "{what}: after a while");
    }

    fn config(&self, edit: impl FnOnce(&mut Config)) {
        let mut config = (*self.state.config.get()).clone();
        edit(&mut config);
        self.state.set_config(Rc::new(config));
        common::pump_for(Duration::from_millis(20));
    }

    fn text(&self) -> String {
        self.editor.document().text()
    }

    fn iter_in(&self, word: &str) -> gtk::TextIter {
        let at = self.text().find(word).expect("the word");
        let chars = self.text()[..at].chars().count() + 1;
        self.editor
            .buffer()
            .iter_at_offset(i32::try_from(chars).unwrap())
    }

    /// The labels of the spelling section's suggestions.
    fn suggestions(&self) -> Vec<String> {
        let menu = self.editor.extra_menu();
        let Some(section) = menu.item_link(0, "section") else {
            return Vec::new();
        };
        let mut labels = Vec::new();
        collect_labels(&section, "spelling.correct", &mut labels);
        labels
    }
}

/// The labels of the items of `model` (and its sections) whose action is `action`.
fn collect_labels(model: &gio::MenuModel, action: &str, out: &mut Vec<String>) {
    for i in 0..model.n_items() {
        let item_action = model
            .item_attribute_value(i, "action", None)
            .and_then(|v| v.get::<String>());
        if item_action.as_deref() == Some(action)
            && let Some(label) = model
                .item_attribute_value(i, "label", None)
                .and_then(|v| v.get::<String>())
        {
            out.push(label);
        }
        for link in ["section", "submenu"] {
            if let Some(sub) = model.item_link(i, link) {
                collect_labels(&sub, action, out);
            }
        }
    }
}

fn write_list(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

// ───────────── Tests ─────────────

const DOC: &str = "\\documentclass{article}
\\usepackage{amsmath}
\\begin{document}
\\section{Introdcution}
Thsi is a sentense with $\\qqmathword + \\alpha_{qqsub}$ and \\qqcommand{} here.
See \\ref{qqref}, \\cite{qqcite} and \\label{qqlabel} with \\emph{emphasisd} text.
\\begin{qqenvname}
% a commment with mistaks
\\end{qqenvname}
\\[ qqdisplay \\]
\\textbf{boldd} \\footnote{footnoet} \\verb!qqverb!
By Lemmma~\\ref{qqlem} and a wordd\\footnote{Fine.} or $x$ and smal$y$.
\\end{document}
";

/// Misspellings in text (and in text arguments such as `\emph`) are underlined; math,
/// commands, labels, references, citations, environment names, comments and verbatim are not.
/// Words right before LaTeX (`emphasisd}`, `Lemmma~`, `wordd\footnote`, `smal$`), which
/// libspelling itself skips, are underlined too.
fn text_and_latex() {
    let f = fixture("text", "main.tex", DOC, config(Vec::new()));
    f.expect_flagged(
        &[
            "Introdcution",
            "Thsi",
            "sentense",
            "emphasisd",
            "boldd",
            "footnoet",
            "Lemmma",
            "wordd",
            "smal",
        ],
        "the fixture",
    );
    // Typing a new misspelling in text, and one in math.
    let buffer = f.editor.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, "Anothr line $qqtyped$.\n");
    buffer.place_cursor(&buffer.end_iter());
    f.expect_flagged(
        &[
            "Introdcution",
            "Thsi",
            "sentense",
            "emphasisd",
            "boldd",
            "footnoet",
            "Lemmma",
            "wordd",
            "smal",
            "Anothr",
        ],
        "after typing",
    );
    // Correcting a word right before `}` removes its underline.
    let at = f.text().find("emphasisd").unwrap();
    let start = buffer.iter_at_offset(i32::try_from(f.text()[..at].chars().count()).unwrap());
    let mut end = start;
    end.forward_chars(9);
    let (mut start, mut end) = (start, end);
    buffer.delete(&mut start, &mut end);
    buffer.insert(&mut start, "emphasis");
    buffer.place_cursor(&buffer.end_iter());
    f.expect_flagged(
        &[
            "Introdcution",
            "Thsi",
            "sentense",
            "boldd",
            "footnoet",
            "Lemmma",
            "wordd",
            "smal",
            "Anothr",
        ],
        "after correcting",
    );
    // A word being typed right before `}` is underlined once the cursor leaves it.
    let at = f.text().find("Anothr").unwrap();
    let mut iter = buffer.iter_at_offset(i32::try_from(f.text()[..at].chars().count()).unwrap());
    buffer.insert(&mut iter, "\\emph{}");
    iter.backward_char();
    buffer.place_cursor(&iter);
    for c in ["q", "q", "t", "y", "p", "d"] {
        let mut cursor = buffer.iter_at_mark(&buffer.get_insert());
        buffer.insert(&mut cursor, c);
        common::pump_for(Duration::from_millis(20));
    }
    common::pump_for(Duration::from_millis(300));
    assert!(
        !spell::misspelled(&f.editor).contains(&"qqtypd".to_owned()),
        "not while it is typed"
    );
    buffer.place_cursor(&buffer.end_iter());
    f.expect_flagged(
        &[
            "Introdcution",
            "Thsi",
            "sentense",
            "boldd",
            "footnoet",
            "Lemmma",
            "wordd",
            "smal",
            "qqtypd",
            "Anothr",
        ],
        "after the cursor left",
    );
    // An edit right before a misspelled word keeps its underline (libspelling clears the word
    // after an edit and skips it when checking again).
    let at = f.text().find("sentense").unwrap();
    let mut iter = buffer.iter_at_offset(i32::try_from(f.text()[..at].chars().count()).unwrap());
    buffer.insert(&mut iter, "(q) ");
    buffer.place_cursor(&buffer.end_iter());
    common::pump_for(Duration::from_millis(300));
    f.expect_flagged(
        &[
            "Introdcution",
            "Thsi",
            "sentense",
            "boldd",
            "footnoet",
            "Lemmma",
            "wordd",
            "smal",
            "qqtypd",
            "Anothr",
        ],
        "after an edit before a misspelled word",
    );
}

/// Words of `spell.word_lists` are accepted: split on blanks and hyphens, with `'s` (and `’s`)
/// for capitalised names; a lower-case entry in any capitalisation, a capitalised one not in
/// lower case. The lists load on a config reload too.
fn word_lists() {
    let doc = "Qqgroth's work on qqcolimit and Qqcolimit, QQCOLIMIT, Qqbarr Qqbeck, Qqmac Qqlane,
qqinfin, Qqgroth again, Qqgroth’s too, but qqgroth is wrong and Qqother too.
";
    let f = fixture("lists", "main.tex", doc, config(Vec::new()));
    // Without the list (and first in this process): all of them.
    let all = [
        "Qqgroth",
        "qqcolimit",
        "Qqcolimit",
        "QQCOLIMIT",
        "Qqbarr",
        "Qqbeck",
        "Qqmac",
        "Qqlane",
        "qqinfin",
        "Qqgroth",
        "Qqgroth’s",
        "qqgroth",
        "Qqother",
    ];
    // (libspelling reads `Qqgroth's` as `Qqgroth` and `s`, and `Qqgroth’s` as one word.)
    f.expect_flagged(&all, "without the list");
    // The list, as the user's Neovim dictionaries are written.
    let list = write_list(
        &f.dir,
        "math.txt",
        "Qqgroth\nqqcolimit\nQqbarr-Qqbeck\nQqmac Qqlane\nA-qqinfin\n",
    );
    f.config(|c| c.spell.word_lists = vec![list]);
    f.expect_flagged(&["qqgroth", "Qqother"], "with the list");
    // A list added later is read on the next reload.
    let more = write_list(&f.dir, "more.txt", "Qqother\n");
    let first = f.dir.join("math.txt");
    f.config(|c| c.spell.word_lists = vec![first, more]);
    f.expect_flagged(&["qqgroth"], "with a second list");
}

/// The spelling menu is the first section of the context menu; a right click (or the Menu key)
/// fills in the suggestions for the word; its actions correct and ignore words.
fn context_menu() {
    let f = fixture(
        "menu",
        "main.tex",
        "Thsi sentence has a qqzzword and wrongg spelling.\n",
        config(Vec::new()),
    );
    f.expect_flagged(&["Thsi", "qqzzword", "wrongg"], "the fixture");
    let adapter = spell::adapter(&f.editor).expect("an adapter");
    let menu = f.editor.extra_menu();
    assert_eq!(
        menu.item_link(0, "section"),
        Some(adapter.menu_model()),
        "spelling is the first section"
    );
    // A right click on `Thsi`: the cursor moves there and the suggestions appear.
    let at = f.iter_in("Thsi");
    spell::prepare_context_menu(&f.editor, Some(&at));
    common::pump_for(Duration::from_millis(50));
    let cursor = f
        .editor
        .buffer()
        .iter_at_mark(&f.editor.buffer().get_insert());
    assert_eq!(cursor.offset(), at.offset());
    let suggestions = f.suggestions();
    assert!(
        suggestions.iter().any(|s| s == "This"),
        "suggestions {suggestions:?}"
    );
    // Correct it through the view's `spelling` actions.
    f.editor
        .view()
        .activate_action("spelling.correct", Some(&"This".to_variant()))
        .expect("the spelling actions are on the view");
    common::pump_for(Duration::from_millis(50));
    assert!(f.text().starts_with("This sentence"), "{}", f.text());
    // The Menu key prepares the suggestions for the word at the cursor.
    let at = f.iter_in("wrongg");
    f.editor.buffer().place_cursor(&at);
    assert!(spell::menu_key_pressed(
        &f.editor,
        gdk::Key::Menu,
        gdk::ModifierType::empty()
    ));
    assert!(spell::menu_key_pressed(
        &f.editor,
        gdk::Key::F10,
        gdk::ModifierType::SHIFT_MASK
    ));
    assert!(!spell::menu_key_pressed(
        &f.editor,
        gdk::Key::F10,
        gdk::ModifierType::empty()
    ));
    let controller = (0..f.editor.view().observe_controllers().n_items())
        .filter_map(|i| f.editor.view().observe_controllers().item(i))
        .filter_map(|o| o.downcast::<gtk::EventControllerKey>().ok())
        .any(|c| c.name().as_deref() == Some("lemma-spell-menu-key"));
    assert!(controller, "the view watches the Menu key");
    common::pump_for(Duration::from_millis(50));
    let suggestions = f.suggestions();
    assert!(
        suggestions.iter().any(|s| s == "wrong"),
        "suggestions {suggestions:?}"
    );
    // Ignore `qqzzword` for the session.
    let at = f.iter_in("qqzzword");
    spell::prepare_context_menu(&f.editor, Some(&at));
    f.editor
        .view()
        .activate_action("spelling.ignore", None)
        .expect("ignore");
    f.editor
        .buffer()
        .place_cursor(&f.editor.buffer().end_iter());
    f.expect_flagged(&["wrongg"], "after Ignore");
}

/// `win.toggle-spell` switches checking for the window; `spell.enabled` and `spell.language`
/// apply on a config reload.
fn toggle_and_reload() {
    let f = fixture(
        "toggle",
        "main.tex",
        "A qqtoggled wordd.\n",
        config(Vec::new()),
    );
    f.expect_flagged(&["qqtoggled", "wordd"], "on");
    let action = f
        .state
        .window
        .lookup_action(spell::TOGGLE_ACTION)
        .expect("win.toggle-spell");
    assert_eq!(action.state().and_then(|s| s.get::<bool>()), Some(true));
    let in_menu = (0..f.state.header.menu_view.n_items()).any(|i| {
        f.state
            .header
            .menu_view
            .item_attribute_value(i, "action", None)
            .and_then(|v| v.get::<String>())
            .as_deref()
            == Some("win.toggle-spell")
    });
    assert!(in_menu, "Check Spelling is in the main menu");
    let sections = f.editor.extra_menu().n_items();
    let tag = spell::adapter(&f.editor)
        .and_then(|a| a.tag())
        .expect("the tag");

    // Off: no adapter, no underlines, no menu section, no actions.
    action.activate(None);
    common::pump_for(Duration::from_millis(20));
    assert_eq!(action.state().and_then(|s| s.get::<bool>()), Some(false));
    assert!(spell::adapter(&f.editor).is_none());
    let buffer = f.editor.buffer();
    let mut start = buffer.start_iter();
    assert!(
        !start.starts_tag(Some(&tag)) && !start.forward_to_tag_toggle(Some(&tag)),
        "no underline is left"
    );
    assert_eq!(f.editor.extra_menu().n_items(), sections - 1);
    assert!(
        f.editor
            .view()
            .activate_action("spelling.add", None)
            .is_err()
    );
    // On again.
    action.activate(None);
    f.expect_flagged(&["qqtoggled", "wordd"], "on again");
    assert_eq!(f.editor.extra_menu().n_items(), sections);

    // `spell.enabled = false` in the config switches it off, `true` on.
    f.config(|c| c.spell.enabled = false);
    assert!(spell::adapter(&f.editor).is_none());
    assert_eq!(action.state().and_then(|s| s.get::<bool>()), Some(false));
    f.config(|c| c.spell.enabled = true);
    f.expect_flagged(&["qqtoggled", "wordd"], "enabled by the config");

    // Another installed language, if there is one.
    let provider = libspelling::Provider::default();
    let languages = provider.list_languages();
    let other = (0..languages.n_items())
        .filter_map(|i| languages.item(i))
        .filter_map(|o| o.downcast::<libspelling::Language>().ok())
        .filter_map(|l| l.code())
        .map(|c| c.to_string())
        .find(|c| c != "en_US" && provider.supports_language(c));
    if let Some(other) = other {
        f.config(|c| c.spell.language = other.clone());
        common::pump_until(TIMEOUT, || {
            spell::language(&f.editor).as_deref() == Some(other.as_str())
        });
    } else {
        println!("only en_US is installed: language switch not checked");
    }
}

/// A language without a dictionary falls back to the default one.
fn missing_language() {
    let mut config = config(Vec::new());
    config.spell.language = "xx_YY".into();
    let f = fixture("missing", "main.tex", "A wrongg word.\n", config);
    f.expect_flagged(&["wrongg"], "with the fallback");
    let fallback = libspelling::Provider::default()
        .default_code()
        .map(|c| c.to_string());
    assert_eq!(spell::language(&f.editor), fallback);
}

/// Nothing of spell checking exists before the editor's first frame.
fn lazy() {
    let dir = common::temp_dir("lazy");
    std::fs::write(dir.join("main.tex"), "A wrongg word.\n").unwrap();
    let mut config = config(Vec::new());
    config.agent.adapter_dir = dir.join(".no-adapter");
    let app = common::application();
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&app, Rc::new(config), None, ws, true);
    // Not presented yet: no frame is drawn.
    let editor = block_on(state.open_file(&dir.join("main.tex"))).expect("open");
    common::pump_for(Duration::from_millis(50));
    assert!(
        spell::adapter(&editor).is_none(),
        "no adapter before a frame"
    );
    assert!(state.window.lookup_action(spell::TOGGLE_ACTION).is_none());
    state.window.set_default_size(900, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || spell::is_checking(&editor));
    assert!(state.window.lookup_action(spell::TOGGLE_ACTION).is_some());
    state.window.destroy();
    common::pump_for(Duration::from_millis(20));
}

/// `.bib` files are not checked.
/// libspelling checks the whole buffer, also lines the highlighter never tagged (they were
/// never on screen): LaTeX there is not underlined either, prose there is, and a change that
/// turns those lines into a comment (or back) without editing them is followed.
fn offscreen_latex() {
    const PLAIN: &str = "Plain words only here.\n";
    let mut text =
        String::from("\\documentclass{article}\n\\begin{document}\nOnscreen qqvisiblez text.\n");
    text.push_str(&PLAIN.repeat(200));
    let first = text.matches('\n').count() as u32;
    text.push_str(
        "Prose qqoffscreenz with \\qqcmdword{} and $\\qqmathword + qqsubword$ here.\n\
See \\ref{qqrefword}, \\cite{qqcitekey} and \\label{qqlabelword} there.\n\
% qqcommentword here\n\
\\begin{qqenvword}\n\
\\end{qqenvword}\n\
\\verb|qqverbword| and \\emph{qqemphz} end.\n\
\\[ qqdisplayword \\] \\textbf{fine} \\hyperref[qqhyperword]{fine}.\n",
    );
    let last = text.matches('\n').count() as u32;
    text.push_str(&PLAIN.repeat(200));
    text.push_str("\\end{document}\n");
    let f = fixture("offscreen", "main.tex", &text, config(Vec::new()));
    let all = ["qqvisiblez", "qqoffscreenz", "qqemphz"];
    f.expect_flagged(&all, "the whole buffer");
    let h = highlight::highlighter(&f.editor).expect("highlighted");
    assert!(
        (first..last).all(|l| !h.is_tagged(l)),
        "the middle lines were never tagged: {:?}",
        h.tagged_lines()
    );
    // A comment environment opened near the top (on screen) runs to the end of the document.
    let buffer = f.editor.buffer();
    let mut at = buffer.iter_at_line(3).unwrap();
    buffer.insert(&mut at, "\\begin{comment}\n");
    buffer.place_cursor(&buffer.end_iter());
    f.expect_flagged(&["qqvisiblez"], "after opening a comment");
    assert!((first + 1..last + 1).all(|l| !h.is_tagged(l)));
    // And closed again.
    let mut start = buffer.iter_at_line(3).unwrap();
    let mut end = buffer.iter_at_line(4).unwrap();
    buffer.delete(&mut start, &mut end);
    buffer.place_cursor(&buffer.end_iter());
    f.expect_flagged(&all, "after removing it");
}

fn bib() {
    let f = fixture(
        "bib",
        "refs.bib",
        "@book{qqkey, author = {Qqauthor}, title = {Wrongg}}\n",
        config(Vec::new()),
    );
    common::pump_for(Duration::from_millis(300));
    assert!(spell::adapter(&f.editor).is_none());
}

/// `detach` (closing the tab) removes the adapter, its underlines, menu section and actions;
/// attaching again works.
fn detach() {
    let f = fixture("detach", "main.tex", "A wrongg word.\n", config(Vec::new()));
    f.expect_flagged(&["wrongg"], "attached");
    let tag = spell::adapter(&f.editor)
        .and_then(|a| a.tag())
        .expect("the tag");
    let sections = f.editor.extra_menu().n_items();
    let controllers = f.editor.view().observe_controllers().n_items();
    let layer = f.editor.view().key_layer().expect("a key layer");
    let handlers = layer.len();
    spell::detach(&f.state, &f.editor);
    assert!(spell::adapter(&f.editor).is_none());
    assert_eq!(f.editor.extra_menu().n_items(), sections - 1);
    assert_eq!(
        f.editor.view().observe_controllers().n_items(),
        controllers - 2,
        "the right-click gesture and the Menu-key controller"
    );
    assert_eq!(layer.len(), handlers, "nothing in the key layer");
    let buffer = f.editor.buffer();
    let mut start = buffer.start_iter();
    assert!(!start.starts_tag(Some(&tag)) && !start.forward_to_tag_toggle(Some(&tag)));
    assert!(
        f.editor
            .view()
            .activate_action("spelling.add", None)
            .is_err()
    );
    // Twice is harmless; attaching again checks again.
    spell::detach(&f.state, &f.editor);
    spell::attach_after_first_frame(&f.state, &f.editor);
    f.expect_flagged(&["wrongg"], "attached again");
    // Closing the tab detaches it.
    let editor = f.editor.clone();
    f.state.close_editor(&editor, true);
    common::pump_for(Duration::from_millis(50));
    assert!(spell::adapter(&editor).is_none());
}
