//! Editor fixes of the improvement pass that the other test files do not cover: files dropped
//! into a CRLF document get CRLF line breaks (`paste::drop_files`), and the paths that drops
//! and pasted images insert are relative to the directory LaTeX resolves them against (the
//! root document's, an `\import`'s, a subfile's own; `paste::path_base`), and so are the file
//! names completion proposes.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_improve_editor`.

mod common;

use adw::prelude::*;
use gtk::gdk;
use lemma_app::editor::{Editor, completion};
use lemma_app::paste;
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::ProjectRoot;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

fn main() -> ExitCode {
    common::run(&[
        (
            "drops_into_a_crlf_document_keep_its_line_breaks",
            drops_keep_crlf,
        ),
        (
            "dropped_and_pasted_paths_are_relative_to_the_root_directory",
            paths_from_the_root,
        ),
        (
            "file_names_are_completed_from_the_directory_latex_reads_them_from",
            completed_file_names,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(10);

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn window(root: &Path) -> Rc<WindowState> {
    let mut config = Config::default();
    config.build.on_save = false;
    config.spell.enabled = false;
    config.history.enabled = false;
    let ws = Workspace::new(ProjectRoot::new(root).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    state
}

fn open(state: &Rc<WindowState>, path: &Path) -> Rc<Editor> {
    block_on(state.open_file(path)).expect("open")
}

fn close(state: &Rc<WindowState>) {
    for editor in state.editors() {
        state.close_editor(&editor, true);
    }
    state.window.destroy();
    common::pump_for(Duration::from_millis(30));
}

/// The lines a drop inserts take the document's `\r\n`, wherever they go: inside a line (from
/// the start of the next one), at the end of a last line without a line break, and at the end.
fn drops_keep_crlf() {
    let root = common::temp_dir("drop-crlf");
    write(
        &root,
        &[
            ("main.tex", "A\r\nB"),
            ("img/plot.png", ""),
            ("two.tex", ""),
        ],
    );
    let state = window(&root);
    let editor = open(&state, &root.join("main.tex"));
    let files: Vec<PathBuf> = ["img/plot.png", "two.tex"]
        .iter()
        .map(|f| root.join(f))
        .collect();
    // Inside the first line: from the start of the next one.
    assert!(paste::drop_files(&state, &editor, &files, 1));
    assert_eq!(
        editor.document().text(),
        "A\r\n\\includegraphics[width=0.8\\textwidth]{img/plot.png}\r\n\\input{two}\r\nB"
    );
    editor.buffer().undo();
    assert_eq!(editor.document().text(), "A\r\nB");
    // Inside the last line, which has no line break: after one.
    assert!(paste::drop_files(&state, &editor, &files[1..], 4));
    assert_eq!(editor.document().text(), "A\r\nB\r\n\\input{two}");
    editor.buffer().undo();
    // At the start of a line.
    assert!(paste::drop_files(&state, &editor, &files[1..], 3));
    assert_eq!(editor.document().text(), "A\r\n\\input{two}\r\nB");
    assert!(
        !editor.document().text().replace("\r\n", "").contains('\n'),
        "{:?}",
        editor.document().text()
    );
    close(&state);
}

fn red_texture() -> gdk::Texture {
    let pixels: Vec<u8> = [255u8, 0, 0, 255].repeat(6);
    gdk::MemoryTexture::new(
        3,
        2,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from(&pixels),
        12,
    )
    .upcast()
}

/// LaTeX reads `\includegraphics`, `\input` and `\bibliography` paths from the root document's
/// directory, so a file dropped into a chapter in a subfolder gets its path from there; below
/// an `\import` from the import directory, in a subfiles document from its own directory (the
/// subfiles package imports it), and with no known root from the file's own directory, as
/// before. A pasted image is saved next to the chapter and inserted with the same rule.
fn paths_from_the_root() {
    let root = common::temp_dir("drop-root");
    write(
        &root,
        &[
            (
                "main.tex",
                "\\documentclass{book}\n\\usepackage{graphicx}\n\\begin{document}\n\
                 \\include{chapters/intro}\n\\subimport{parts/}{p}\n\\end{document}\n",
            ),
            ("chapters/intro.tex", "Intro.\n"),
            ("chapters/sec.tex", ""),
            ("parts/p.tex", "Part.\n"),
            ("parts/fig.png", ""),
            (
                "sub/s.tex",
                "\\documentclass[../main]{subfiles}\n\\begin{document}\n\\end{document}\n",
            ),
            ("loose/notes.tex", "Notes.\n"),
            ("img/plot.png", ""),
            ("refs.bib", ""),
        ],
    );
    let state = window(&root);
    let main = open(&state, &root.join("main.tex"));
    let intro = open(&state, &root.join("chapters/intro.tex"));
    completion::refresh_project(&state);
    common::pump_until(TIMEOUT, || !completion::project_busy(&state));
    let drop = |editor: &Rc<Editor>, file: &str| -> String {
        let before = editor.document().text();
        assert!(
            paste::drop_files(&state, editor, &[root.join(file)], 0),
            "{file}"
        );
        let text = editor.document().text();
        editor.buffer().undo();
        assert_eq!(editor.document().text(), before);
        text.lines().next().unwrap().to_owned()
    };
    // A chapter the root includes: from the root's directory.
    assert_eq!(
        drop(&intro, "img/plot.png"),
        "\\includegraphics[width=0.8\\textwidth]{img/plot.png}"
    );
    assert_eq!(drop(&intro, "chapters/sec.tex"), "\\input{chapters/sec}");
    assert_eq!(drop(&intro, "refs.bib"), "\\bibliography{refs}");
    // The root itself.
    assert_eq!(drop(&main, "chapters/sec.tex"), "\\input{chapters/sec}");
    // Below an `\import`: from the import directory.
    let part = open(&state, &root.join("parts/p.tex"));
    assert_eq!(
        drop(&part, "parts/fig.png"),
        "\\includegraphics[width=0.8\\textwidth]{fig.png}"
    );
    // A subfiles document: from its own directory.
    let sub = open(&state, &root.join("sub/s.tex"));
    assert_eq!(
        drop(&sub, "img/plot.png"),
        "\\includegraphics[width=0.8\\textwidth]{../img/plot.png}"
    );
    // A file below the root's directory that nothing includes (yet) belongs to the project of
    // the root found for the window (the build's), as for the math preview's preamble.
    common::pump_until(TIMEOUT, || {
        state.build.get().and_then(|b| b.root())
            == lemma_core::canonicalize(root.join("main.tex")).ok()
    });
    let loose = open(&state, &root.join("loose/notes.tex"));
    assert_eq!(
        drop(&loose, "img/plot.png"),
        "\\includegraphics[width=0.8\\textwidth]{img/plot.png}"
    );
    // A file outside it, with no root of its own: from its own directory, as before.
    let elsewhere = common::temp_dir("drop-root-elsewhere");
    write(
        &elsewhere,
        &[("notes/n.tex", "Notes.\n"), ("img/plot.png", "")],
    );
    let alone = open(&state, &elsewhere.join("notes/n.tex"));
    assert_eq!(
        paste::drop_command(
            &paste::path_base(&state, alone.document()),
            &elsewhere.join("img/plot.png"),
            false
        )
        .as_deref(),
        Some("\\includegraphics[width=0.8\\textwidth]{../img/plot.png}")
    );
    // `% !TEX root` names the root.
    let magic = elsewhere.join("notes/magic.tex");
    let name = root.file_name().unwrap().to_str().unwrap();
    let text = format!("% !TEX root = ../../{name}/main.tex\nText.\n");
    std::fs::write(&magic, text).unwrap();
    let magic = open(&state, &magic);
    assert_eq!(
        paste::path_base(&state, magic.document()),
        lemma_core::canonicalize(&root).unwrap()
    );

    // A pasted image goes to `figures/` next to the chapter, and in from the root's directory.
    state.show_document(intro.document());
    intro.place_cursor_at(0);
    let view = intro.view();
    view.clipboard().set_texture(&red_texture());
    view.emit_paste_clipboard();
    common::pump_until(TIMEOUT, || paste::name_prompt(&state).is_some());
    let (dialog, entry) = paste::name_prompt(&state).unwrap();
    entry.set_text("shot");
    let save = dialog
        .default_widget()
        .and_downcast::<gtk::Button>()
        .expect("Save");
    save.emit_clicked();
    common::pump_until(TIMEOUT, || {
        intro.document().text().contains("includegraphics")
    });
    assert_eq!(
        intro.document().text(),
        "\\includegraphics[width=0.8\\textwidth]{chapters/figures/shot}Intro.\n"
    );
    assert!(root.join("chapters/figures/shot.png").is_file());
    close(&state);
}

/// Completion proposes a file under the name its document has to write: from the root's
/// directory in the root and the chapters it includes, from the import directory below an
/// `\import`, and from its own directory in a subfiles document (as dropped files are named).
fn completed_file_names() {
    let root = common::temp_dir("complete-file-names");
    write(
        &root,
        &[
            (
                "main.tex",
                "\\documentclass{book}\n\\usepackage{graphicx}\n\\begin{document}\n\
                 \\include{chapters/intro}\n\\subimport{parts/}{p}\n\\end{document}\n",
            ),
            ("chapters/intro.tex", "Intro.\n"),
            ("parts/p.tex", "Part.\n"),
            ("parts/fig.png", ""),
            (
                "sub/s.tex",
                "\\documentclass[../main]{subfiles}\n\\begin{document}\nSub.\n\\end{document}\n",
            ),
            ("img/plot.png", ""),
        ],
    );
    let state = window(&root);
    let main = open(&state, &root.join("main.tex"));
    let intro = open(&state, &root.join("chapters/intro.tex"));
    let part = open(&state, &root.join("parts/p.tex"));
    let sub = open(&state, &root.join("sub/s.tex"));
    completion::refresh_project(&state);
    common::pump_until(TIMEOUT, || !completion::project_busy(&state));
    // The names proposed in `\includegraphics{` on a line of its own, after the first line that
    // is plain text (or at the end).
    let names = |editor: &Rc<Editor>, wanted: &str| -> Vec<String> {
        state.show_document(editor.document());
        let buffer = editor.buffer();
        let text = editor.document().text();
        let line = text
            .lines()
            .position(|l| !l.starts_with('\\'))
            .map_or(buffer.line_count() - 1, |l| l as i32);
        let mut at = buffer.iter_at_line(line).unwrap();
        buffer.begin_user_action();
        buffer.insert(&mut at, "\\includegraphics{\n");
        buffer.end_user_action();
        let mut cursor = buffer.iter_at_line(line).unwrap();
        cursor.forward_to_line_end();
        buffer.place_cursor(&cursor);
        let labels = || -> Vec<String> {
            completion::items_at_cursor(&state, editor)
                .into_iter()
                .map(|i| i.label)
                .collect()
        };
        common::pump_until(TIMEOUT, || labels().iter().any(|l| l == wanted));
        let found = labels();
        buffer.undo();
        assert_eq!(editor.document().text(), text);
        found
    };
    let has = |names: &[String], name: &str| names.iter().any(|n| n == name);
    // The root, and a chapter it includes: from the root's directory.
    for editor in [&main, &intro] {
        let found = names(editor, "img/plot.png");
        assert!(
            has(&found, "img/plot.png") && has(&found, "parts/fig.png"),
            "{found:?}"
        );
    }
    // Below `\subimport{parts/}`: from the import directory.
    let found = names(&part, "fig.png");
    assert!(
        has(&found, "fig.png") && has(&found, "../img/plot.png") && !has(&found, "parts/fig.png"),
        "{found:?}"
    );
    // A subfiles document: from its own directory.
    let found = names(&sub, "../img/plot.png");
    assert!(
        has(&found, "../img/plot.png") && has(&found, "../parts/fig.png"),
        "{found:?}"
    );
    // And back in the chapter: from the root's again.
    let found = names(&intro, "img/plot.png");
    assert!(
        has(&found, "img/plot.png") && !has(&found, "../img/plot.png"),
        "{found:?}"
    );
    close(&state);
}
