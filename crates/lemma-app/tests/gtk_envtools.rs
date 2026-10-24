//! M5 WP5.3: environment tools (`editor/envtools.rs`), the align helper (`editor/align.rs`)
//! and latexindent formatting (`format.rs`) in a window: live rename (one undo step with the
//! typing), the actions and their keys, the Change Environment popover, and Format Document
//! applied as reviewable hunks that reject cleanly, with a fake latexindent.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_envtools`.
//! `LEMMA_TEST_SNAPSHOTS=dir` saves `envtools-change-env.png` (the popover) and
//! `align-columns.png` there.

mod common;

use adw::prelude::*;
use lemma_app::editor::{Editor, align, completion, envtools};
use lemma_app::format;
use lemma_app::review_ui::{self, Decision};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::{Origin, ProjectRoot, TextEdit};
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    isolate_environment();
    common::run(&[
        ("live_rename_is_one_undo_step", live_rename),
        ("live_rename_ends_outside_the_name", live_rename_ends),
        ("change_environment_popover", change_environment),
        ("toggle_star_and_delete_environment", star_and_delete),
        ("align_insert_row_add_column", align_actions),
        ("actions_are_in_the_palette_with_keys", palette_entries),
        ("format_document_as_reviewable_hunks", format_document),
        ("format_selection", format_selection),
        ("missing_latexindent_hides_format", missing_latexindent),
        // Final review (M5)
        (
            "regress5_live_rename_then_environment_completion",
            regress5_rename_completion,
        ),
        (
            "regress5_failing_latexindent_keeps_format_and_explains",
            regress5_failing_latexindent,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(30);

/// Points the XDG directories, the config file and the temporary directory (latexindent's
/// working directories) into a directory under `target`. Runs before GTK starts any thread.
fn isolate_environment() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-envtools-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("tmp")).unwrap();
    // SAFETY: single-threaded: GTK is not initialised yet and no thread was spawned.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_STATE_HOME", root.join("state"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        std::env::set_var("LEMMA_CONFIG", root.join("config.toml"));
        std::env::set_var("TMPDIR", root.join("tmp"));
    }
}

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

/// A window on a project with `main.tex` = `text`; `latexindent` is the fake to use (none:
/// a program that does not exist).
fn fixture(name: &str, text: &str, latexindent: Option<&str>) -> Fixture {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
    let dir = common::temp_dir(name);
    std::fs::write(dir.join("main.tex"), text).unwrap();
    let program = match latexindent {
        Some(script) => {
            let path = dir.join("fake-latexindent");
            std::fs::write(&path, script).unwrap();
            #[cfg(unix)]
            let mut perm = std::fs::metadata(&path).unwrap().permissions();
            #[cfg(unix)]
            std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
            #[cfg(unix)]
            std::fs::set_permissions(&path, perm).unwrap();
            path
        }
        None => dir.join("no-latexindent"),
    };
    format::set_program(Some((program, Duration::from_secs(10))));
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = false;
    config.spell.enabled = false;
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    Fixture { state, dir }
}

impl Fixture {
    fn open(&self) -> Rc<Editor> {
        let editor = block_on(self.state.open_file(&self.dir.join("main.tex"))).expect("open");
        let view = editor.view().clone();
        common::pump_until(TIMEOUT, || view.is_mapped() && view.height() > 0);
        let doc = editor.document().clone();
        common::pump_until(TIMEOUT, || doc.has_line_states());
        editor
    }

    fn run(&self, action: &str) {
        WidgetExt::activate_action(&self.state.window, action, None)
            .unwrap_or_else(|e| panic!("{action}: {e}"));
        common::pump_for(Duration::from_millis(20));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
        common::pump_for(Duration::from_millis(20));
    }
}

fn offset_of(editor: &Editor, needle: &str, delta: usize) -> usize {
    editor
        .document()
        .text()
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is in the text"))
        + delta
}

/// Types `text` at `offset` as GtkTextView does (one user action per commit).
fn type_at(editor: &Editor, offset: usize, text: &str) {
    assert!(editor.place_cursor_at(offset));
    let buffer = editor.buffer();
    buffer.begin_user_action();
    buffer.insert_interactive_at_cursor(text, true);
    buffer.end_user_action();
    common::pump_for(Duration::from_millis(10));
}

/// Deletes the character before `offset` as Backspace does.
fn backspace_at(editor: &Editor, offset: usize) {
    assert!(editor.place_cursor_at(offset));
    let buffer = editor.buffer();
    let mut end = buffer.iter_at_mark(&buffer.get_insert());
    let mut start = end;
    start.backward_char();
    buffer.begin_user_action();
    buffer.delete_interactive(&mut start, &mut end, true);
    buffer.end_user_action();
    common::pump_for(Duration::from_millis(10));
}

/// `LEMMA_TEST_SNAPSHOTS=dir`: saves the window as `dir/<name>.png`.
fn snapshot(f: &Fixture, name: &str) {
    let Some(dir) = std::env::var_os("LEMMA_TEST_SNAPSHOTS") else {
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join(format!("{name}.png"));
    block_on(lemma_app::testmode::snapshot_next_frame(
        &f.state.window,
        &png,
    ))
    .unwrap();
}

fn undo(editor: &Editor) {
    editor.buffer().undo();
    common::pump_for(Duration::from_millis(10));
}

const ALIGN_DOC: &str = "\\documentclass{article}\n\\begin{document}\n\\begin{align}\n  a &= b \\\\\n  ccc &= d\n\\end{align}\n\\end{document}\n";

fn live_rename() {
    let f = fixture("live-rename", ALIGN_DOC, None);
    let editor = f.open();
    let doc = editor.document().clone();
    // A star typed at the end of the `\begin` name: the `\end` follows, in the same undo step.
    type_at(&editor, offset_of(&editor, "{align}", 6), "*");
    assert!(doc.text().contains("\\begin{align*}"));
    assert!(doc.text().contains("\\end{align*}"), "{}", doc.text());
    assert!(envtools::linked(&editor).is_some());
    undo(&editor);
    assert_eq!(doc.text(), ALIGN_DOC);
    // Editing the `\end` name renames the `\begin`; several keystrokes keep the link.
    // (Renaming the `\begin` moves the `\end`: its name is looked up again each time.)
    let end_name = |e: &Editor| {
        let text = e.document().text();
        let start = text.find("\\end{al").unwrap() + 5;
        start..start + text[start..].find('}').unwrap()
    };
    backspace_at(&editor, end_name(&editor).end); // align → alig
    backspace_at(&editor, end_name(&editor).end); // alig → ali
    type_at(&editor, end_name(&editor).end, "gned");
    let text = doc.text();
    assert!(text.contains("\\begin{aligned}"), "{text}");
    assert!(text.contains("\\end{aligned}"), "{text}");
    // Deleting the whole name and typing a new one.
    let at = offset_of(&editor, "\\begin{aligned}", 7);
    let buffer = editor.buffer();
    let (mut a, mut b) = (
        doc.iter_at_byte(at).unwrap(),
        doc.iter_at_byte(at + 7).unwrap(),
    );
    buffer.begin_user_action();
    buffer.delete_interactive(&mut a, &mut b, true);
    buffer.end_user_action();
    common::pump_for(Duration::from_millis(10));
    assert!(doc.text().contains("\\begin{}"));
    type_at(&editor, at, "gather");
    let text = doc.text();
    assert!(
        text.contains("\\begin{gather}") && text.contains("\\end{gather}"),
        "{text}"
    );
    // Undo goes back step by step; each step keeps both names equal.
    for _ in 0..12 {
        undo(&editor);
        let text = doc.text();
        let begin = text.split("\\begin{").nth(2).unwrap().split('}').next();
        let end = text.split("\\end{").nth(1).unwrap().split('}').next();
        assert_eq!(begin, end, "{text}");
    }
    assert_eq!(doc.text(), ALIGN_DOC);
    // Undo itself never renames.
    assert!(envtools::linked(&editor).is_none());
}

fn live_rename_ends() {
    let text = "\\begin{document}\n\\begin{proof}\nEasy.\n\\end{proof}\n\\begin{x}\n\\end{y}\n\\end{document}\n";
    let f = fixture("live-rename-ends", text, None);
    let editor = f.open();
    let doc = editor.document().clone();
    type_at(&editor, offset_of(&editor, "{proof}", 6), "s");
    assert!(doc.text().contains("\\end{proofs}"));
    // An edit outside the name ends the link; the names stay as they are.
    type_at(&editor, offset_of(&editor, "Easy.", 5), " Done.");
    assert!(envtools::linked(&editor).is_none());
    // Invalid names are not copied: a space ends the link, the partner keeps its name.
    type_at(&editor, offset_of(&editor, "{proofs}", 7), " x");
    assert!(doc.text().contains("\\end{proofs}"), "{}", doc.text());
    // An unmatched `\begin{x}` (closed by `\end{y}`) has no partner: nothing else changes.
    let before = doc.text();
    type_at(&editor, offset_of(&editor, "{x}", 2), "z");
    assert_eq!(
        doc.text(),
        before.replacen("\\begin{x}", "\\begin{xz}", 1),
        "{}",
        doc.text()
    );
    // Programmatic edits are not typing: outside a user action, and through `apply_edits`.
    let at = offset_of(&editor, "\\end{document}", 13);
    let mut iter = doc.iter_at_byte(at).unwrap();
    editor.buffer().insert(&mut iter, "x");
    common::pump_for(Duration::from_millis(30));
    assert!(doc.text().contains("\\begin{document}\n"), "{}", doc.text());
    assert!(doc.text().contains("\\end{documentx}"));
    let at = offset_of(&editor, "\\end{proofs}", 11);
    doc.apply_edits(&[TextEdit::insert(at, "y")], Origin::User)
        .unwrap();
    common::pump_for(Duration::from_millis(30));
    assert!(doc.text().contains("\\end{proofsy}"));
    assert!(doc.text().contains("\\begin{proofs x}"));
}

fn change_environment() {
    let f = fixture("change-env", ALIGN_DOC, None);
    let editor = f.open();
    let doc = editor.document().clone();
    editor.view().grab_focus();
    editor.place_cursor_at(offset_of(&editor, "ccc", 1));
    f.run("win.change-environment");
    let entry = envtools::prompt_entry().expect("the popover is open");
    assert_eq!(entry.text(), "align");
    snapshot(&f, "envtools-change-env");
    // An invalid name changes nothing and keeps the popover.
    entry.set_text("bad name");
    entry.emit_activate();
    common::pump_for(Duration::from_millis(20));
    assert_eq!(doc.text(), ALIGN_DOC);
    assert!(envtools::prompt_entry().is_some());
    entry.set_text("alignat");
    entry.emit_activate();
    common::pump_for(Duration::from_millis(50));
    assert_eq!(doc.text(), ALIGN_DOC.replace("{align}", "{alignat}"));
    assert!(envtools::prompt_entry().is_none());
    undo(&editor);
    assert_eq!(doc.text(), ALIGN_DOC);
    // The innermost environment; outside any, a toast and nothing else.
    editor.place_cursor_at(0);
    assert!(envtools::change_environment_in(&editor, "x").is_err());
    editor.place_cursor_at(offset_of(&editor, "\\end{document}", 0));
    envtools::change_environment_in(&editor, "doc").unwrap();
    assert!(doc.text().contains("\\begin{doc}") && doc.text().contains("\\end{doc}"));
    assert!(doc.text().contains("\\begin{align}"));
    // Typing after a command (an `apply_edits`, in an earlier main-loop iteration) renames.
    common::pump_for(Duration::from_millis(10));
    type_at(&editor, offset_of(&editor, "{align}", 6), "*");
    assert!(doc.text().contains("\\end{align*}"), "{}", doc.text());
}

fn star_and_delete() {
    let f = fixture("star-delete", ALIGN_DOC, None);
    let editor = f.open();
    let doc = editor.document().clone();
    editor.view().grab_focus();
    editor.place_cursor_at(offset_of(&editor, "a &=", 0));
    f.run("win.toggle-environment-star");
    assert_eq!(doc.text(), ALIGN_DOC.replace("{align}", "{align*}"));
    f.run("win.toggle-environment-star");
    assert_eq!(doc.text(), ALIGN_DOC);
    f.run("win.delete-environment");
    assert_eq!(
        doc.text(),
        "\\documentclass{article}\n\\begin{document}\na &= b \\\\\nccc &= d\n\\end{document}\n"
    );
    undo(&editor);
    assert_eq!(doc.text(), ALIGN_DOC);
}

fn align_actions() {
    let text = "\\begin{document}\n\\begin{tabular}{lc}\n  a & b \\\\\n  long cell & x\n\\end{tabular}\n\\end{document}\n";
    let f = fixture("align", text, None);
    let editor = f.open();
    let doc = editor.document().clone();
    editor.view().grab_focus();
    editor.place_cursor_at(offset_of(&editor, "long", 0));
    f.run("win.align-columns");
    let aligned = "\\begin{document}\n\\begin{tabular}{lc}\n  a         & b \\\\\n  long cell & x\n\\end{tabular}\n\\end{document}\n";
    assert_eq!(doc.text(), aligned);
    snapshot(&f, "align-columns");
    undo(&editor);
    assert_eq!(doc.text(), text);
    editor.place_cursor_at(offset_of(&editor, "a & b", 0));
    f.run("win.insert-row-below");
    assert_eq!(
        doc.text(),
        text.replace("  a & b \\\\\n", "  a & b \\\\\n   & \\\\\n")
    );
    assert_eq!(editor.cursor_offset(), offset_of(&editor, "   & \\\\", 2));
    undo(&editor);
    f.run("win.add-column");
    assert_eq!(
        doc.text(),
        text.replace("{lc}", "{lcc}")
            .replace("a & b \\\\", "a & b & \\\\")
            .replace("cell & x", "cell & x &")
    );
    undo(&editor);
    assert_eq!(doc.text(), text);
    // Outside the family: a message, no change.
    editor.place_cursor_at(0);
    assert!(align::align_in(&editor).is_err());
}

fn palette_entries() {
    let f = fixture("palette", ALIGN_DOC, None);
    let _editor = f.open();
    let commands = lemma_app::palette::commands(&f.state);
    let find = |name: &str| {
        commands
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is in the palette"))
    };
    let canon = |accels: &[String]| -> Vec<_> {
        accels
            .iter()
            .map(|a| gtk::accelerator_parse(a).expect("valid"))
            .collect()
    };
    let keys = |name: &str, want: &str| {
        assert_eq!(
            canon(&find(name).accels),
            canon(&[want.to_owned()]),
            "{name}"
        );
    };
    assert_eq!(find("win.change-environment").label, "Change Environment…");
    keys("win.change-environment", "<Control><Shift>e");
    keys("win.toggle-environment-star", "<Control>asterisk");
    keys("win.align-columns", "<Control><Shift>a");
    for name in [
        "win.delete-environment",
        "win.insert-row-below",
        "win.add-column",
    ] {
        find(name);
    }
}

/// A fake latexindent: `--version` succeeds; formatting indents `\item` lines and strips
/// trailing blanks.
const FAKE: &str = "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'fake latexindent'; exit 0; fi\nsed -e 's/[[:space:]]*$//' -e 's/^\\\\item/  \\\\item/'\n";

const LIST_DOC: &str = "\\documentclass{article}\n\\begin{document}\nIntro.   \n\\begin{itemize}\n\\item One\n\\item Two\n\\end{itemize}\n\\end{document}\n";

fn wait_for_detection(f: &Fixture) -> bool {
    common::pump_until(TIMEOUT, || format::detection_done(&f.state).is_some());
    format::detection_done(&f.state).unwrap()
}

fn format_document() {
    let f = fixture("format", LIST_DOC, Some(FAKE));
    assert!(wait_for_detection(&f));
    let editor = f.open();
    let doc = editor.document().clone();
    assert!(f.state.window.lookup_action("format-document").is_some());
    block_on(format::format_in(&f.state, &editor, false)).unwrap();
    let formatted = LIST_DOC
        .replace("Intro.   ", "Intro.")
        .replace("\\item", "  \\item");
    assert_eq!(doc.text(), formatted);
    assert!(
        review_ui::hunk_count(&doc) >= 2,
        "{}",
        review_ui::hunk_count(&doc)
    );
    // On disk too (the file was clean), as after an agent edit.
    assert_eq!(
        std::fs::read_to_string(f.dir.join("main.tex")).unwrap(),
        formatted
    );
    // Rejecting every hunk restores the text exactly.
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || review_ui::hunk_count(&doc) == 0);
    assert_eq!(doc.text(), LIST_DOC);
    // Formatting again, then nothing left to do after keeping it.
    block_on(format::format_in(&f.state, &editor, false)).unwrap();
    review_ui::decide_document(&f.state, &doc, Decision::Accept);
    common::pump_until(TIMEOUT, || review_ui::hunk_count(&doc) == 0);
    assert_eq!(doc.text(), formatted);
    block_on(format::format_in(&f.state, &editor, false)).unwrap();
    assert_eq!(review_ui::hunk_count(&doc), 0);
}

fn format_selection() {
    let f = fixture("format-selection", LIST_DOC, Some(FAKE));
    assert!(wait_for_detection(&f));
    let editor = f.open();
    let doc = editor.document().clone();
    assert!(block_on(format::format_in(&f.state, &editor, true)).is_err());
    // Select from inside `\item One` to inside `\item Two`: whole lines are formatted.
    let buffer = editor.buffer();
    let a = doc.iter_at_byte(offset_of(&editor, "One", 0)).unwrap();
    let b = doc.iter_at_byte(offset_of(&editor, "Two", 1)).unwrap();
    buffer.select_range(&a, &b);
    block_on(format::format_in(&f.state, &editor, true)).unwrap();
    assert_eq!(doc.text(), LIST_DOC.replace("\\item", "  \\item"));
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || review_ui::hunk_count(&doc) == 0);
    assert_eq!(doc.text(), LIST_DOC);
}

fn missing_latexindent() {
    let f = fixture("no-latexindent", LIST_DOC, None);
    assert!(!wait_for_detection(&f));
    let _editor = f.open();
    assert!(f.state.window.lookup_action("format-document").is_none());
    assert!(
        !lemma_app::palette::commands(&f.state)
            .iter()
            .any(|c| c.name.starts_with("win.format-"))
    );
}

// ----- final review (M5) -----

/// Live rename turns `align` into `eq` in both names; accepting `equation` from the
/// environment completion then renames the pair, instead of inserting a second environment.
fn regress5_rename_completion() {
    let f = fixture("r5-rename-complete", ALIGN_DOC, None);
    let editor = f.open();
    let doc = editor.document().clone();
    let start = offset_of(&editor, "\\begin{align}", 7);
    for end in (start + 1..=start + 5).rev() {
        backspace_at(&editor, end);
    }
    type_at(&editor, start, "e");
    type_at(&editor, start + 1, "q");
    let expected = ALIGN_DOC.replace("{align}", "{eq}");
    assert_eq!(doc.text(), expected);
    assert!(envtools::linked(&editor).is_some());
    let items = completion::items_at_cursor(&f.state, &editor);
    let equation = items
        .iter()
        .find(|i| i.label == "equation")
        .unwrap_or_else(|| {
            panic!(
                "equation in {:?}",
                items.iter().map(|i| &i.label).collect::<Vec<_>>()
            )
        })
        .clone();
    completion::accept(&f.state, &editor, &equation);
    common::pump_for(Duration::from_millis(20));
    assert_eq!(doc.text(), ALIGN_DOC.replace("{align}", "{equation}"));
}

/// A latexindent that is installed but fails (here: as with a missing Perl module) keeps the
/// format actions, and running one says why it failed.
fn regress5_failing_latexindent() {
    let broken = "#!/bin/sh\necho \"Can't locate YAML/Tiny.pm in @INC\" >&2\nexit 2\n";
    let f = fixture("r5-broken-latexindent", LIST_DOC, Some(broken));
    assert!(wait_for_detection(&f), "the actions stay");
    assert!(f.state.window.lookup_action("format-document").is_some());
    let editor = f.open();
    let error = block_on(format::format_in(&f.state, &editor, false)).unwrap_err();
    assert!(error.contains("YAML/Tiny.pm"), "{error}");
    assert_eq!(editor.document().text(), LIST_DOC);
}
