//! WP5.5: image paste and drops (`paste.rs`), templates and CLAUDE.md (`templates.rs`), BibTeX
//! import (`bibimport.rs`) and auto-save (`autosave.rs`).
//!
//! Windows get no `Paths` and `LEMMA_CONFIG` is never read: nothing touches the user's
//! configuration or state. The BibTeX import never reaches the network: `LEMMA_DOI_URL` and
//! `LEMMA_ARXIV_URL` point at a fake server on 127.0.0.1 started by `main`.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_qol`.

mod common;

use adw::prelude::*;
use lemma_app::agent::AgentState;
use lemma_app::editor::Editor;
use lemma_app::review_ui::{self, Decision};
use lemma_app::window::WindowState;
use lemma_app::workspace::{Workspace, normalize};
use lemma_app::{autosave, bibimport, paste, templates};
use lemma_config::{Autosave, Config};
use lemma_core::ProjectRoot;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Requests the fake server answered.
static REQUESTS: AtomicUsize = AtomicUsize::new(0);

fn main() -> ExitCode {
    let base = fake_server();
    // SAFETY: no other thread reads the environment yet (the server thread does not).
    unsafe {
        std::env::set_var(bibimport::DOI_URL_ENV, &base);
        std::env::set_var(bibimport::ARXIV_URL_ENV, format!("{base}/api/query"));
    }
    common::run(&[
        ("image_paste_saves_a_png_and_inserts_it", image_paste),
        ("text_paste_is_unchanged", text_paste),
        ("drops_insert_relative_commands", drops),
        ("templates_create_files_and_open_main", templates_dialog),
        ("templates_build_every_file", templates_files),
        ("project_instructions_are_inferred_and_kept", instructions),
        ("citations_are_appended_as_hunks", citations),
        ("citation_dialog_reports_errors", citation_dialog),
        ("autosave_idle_waits_for_agent_turns", autosave_idle),
        ("autosave_focus_and_off", autosave_focus),
        ("regress5_markup_plain_text", regress5_markup),
        ("regress5_drops_go_on_their_own_lines", regress5_drops),
        (
            "regress5_template_outside_the_project_opens_a_new_window",
            regress5_template_elsewhere,
        ),
    ])
}

const TIMEOUT: Duration = Duration::from_secs(20);

const CROSSREF: &str = " @article{Grothendieck_1957, title={Sur quelques points d’algèbre homologique, I}, volume={9}, DOI={10.2748/tmj/1178244839}, journal={Tohoku Mathematical Journal}, author={Grothendieck, Alexander}, year={1957}, month=jan, pages={119–221} }\n";

const ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
  <entry>
    <id>http://arxiv.org/abs/2301.01234v2</id>
    <published>2023-01-03T18:00:00Z</published>
    <title>Stacks for
      Everyone</title>
    <author><name>Ada Lovelace</name></author>
    <arxiv:primary_category term="math.AG" scheme="http://arxiv.org/schemas/atom"/>
  </entry>
</feed>"#;

/// Serves the DOI `10.2748/tmj/1178244839` (via a redirect, as doi.org does) and the arXiv ID
/// `2301.01234`; everything else is a 404.
fn fake_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let redirect = format!("{base}/crossref/tmj");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            let mut accept = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if line.to_ascii_lowercase().starts_with("accept:") {
                    accept = line.trim().to_owned();
                }
            }
            REQUESTS.fetch_add(1, Ordering::SeqCst);
            let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
            let (status, extra, body) = match path.as_str() {
                "/10.2748/tmj/1178244839" => (302, format!("Location: {redirect}\r\n"), ""),
                "/crossref/tmj" if accept.contains("x-bibtex") => (200, String::new(), CROSSREF),
                p if p.starts_with("/api/query?id_list=2301.01234") => (200, String::new(), ATOM),
                _ => (404, String::new(), "not found"),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
            let _ = stream.read(&mut [0; 1]);
        }
    });
    base
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

fn window_with(root: &Path, configure: impl FnOnce(&mut Config)) -> Rc<WindowState> {
    let mut config = Config::default();
    config.build.on_save = false;
    config.spell.enabled = false;
    config.history.enabled = false;
    configure(&mut config);
    let ws = Workspace::new(ProjectRoot::new(root).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(1000, 700);
    state.window.present();
    common::pump_until(TIMEOUT, || state.window.is_mapped());
    state
}

fn window(root: &Path) -> Rc<WindowState> {
    window_with(root, |_| {})
}

fn open(state: &Rc<WindowState>, path: &Path) -> Rc<Editor> {
    block_on(state.open_file(path)).expect("open")
}

fn text_of(editor: &Editor) -> String {
    editor.document().text()
}

fn close(state: &Rc<WindowState>) {
    for editor in state.editors() {
        state.close_editor(&editor, true);
    }
    state.window.destroy();
    common::pump_for(Duration::from_millis(30));
}

// ----- paste and drops -----

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

fn image_paste() {
    let root = common::temp_dir("paste");
    write(
        &root,
        &[("doc/main.tex", "Before\n"), ("doc/figures/fig-1.png", "")],
    );
    let state = window(&root);
    let editor = open(&state, &root.join("doc/main.tex"));
    editor.place_cursor_at(7);
    let view = editor.view();
    view.clipboard().set_texture(&red_texture());

    // A taken name is refused; Cancel inserts and writes nothing.
    view.emit_paste_clipboard();
    common::pump_until(TIMEOUT, || paste::name_prompt(&state).is_some());
    let (dialog, entry) = paste::name_prompt(&state).unwrap();
    assert_eq!(entry.text(), "fig-2", "the first free number");
    assert!(dialog.is_response_enabled(paste::RESPONSE_SAVE));
    entry.set_text("fig-1");
    assert!(!dialog.is_response_enabled(paste::RESPONSE_SAVE));
    entry.set_text("a b");
    assert!(!dialog.is_response_enabled(paste::RESPONSE_SAVE));
    dialog.close();
    common::pump_until(TIMEOUT, || paste::name_prompt(&state).is_none());
    common::pump_for(Duration::from_millis(50));
    assert_eq!(text_of(&editor), "Before\n");

    // Save: figures/plot.png (a real PNG) and the \includegraphics where the cursor was.
    view.emit_paste_clipboard();
    common::pump_until(TIMEOUT, || paste::name_prompt(&state).is_some());
    let (dialog, entry) = paste::name_prompt(&state).unwrap();
    entry.set_text("plot.png");
    // Save is the default response (what Enter in the entry activates in a focused window;
    // the offscreen window never has the focus, so the test clicks it).
    let save = dialog
        .default_widget()
        .and_downcast::<gtk::Button>()
        .expect("Save");
    save.emit_clicked();
    let png = root.join("doc/figures/plot.png");
    common::pump_for(Duration::from_millis(1500));
    common::pump_until(TIMEOUT, || text_of(&editor).contains("includegraphics"));
    assert_eq!(
        text_of(&editor),
        "Before\n\\includegraphics[width=0.8\\textwidth]{figures/plot}"
    );
    let saved = gdk::Texture::from_filename(&png).expect("a PNG");
    assert_eq!((saved.width(), saved.height()), (3, 2));
    // One undo step.
    editor.buffer().undo();
    assert_eq!(text_of(&editor), "Before\n");
    close(&state);
}

fn text_paste() {
    let root = common::temp_dir("paste-text");
    write(&root, &[("main.tex", "x\n")]);
    let state = window(&root);
    let editor = open(&state, &root.join("main.tex"));
    editor.place_cursor_at(0);
    let clipboard = editor.view().clipboard();
    clipboard.set_text("hello ");
    assert!(!paste::clipboard_has_image(&clipboard.formats()));
    editor.view().emit_paste_clipboard();
    common::pump_until(TIMEOUT, || text_of(&editor) == "hello x\n");
    assert!(paste::name_prompt(&state).is_none());
    clipboard.set_texture(&red_texture());
    assert!(paste::clipboard_has_image(&clipboard.formats()));
    close(&state);
}

fn drops() {
    let root = common::temp_dir("drops");
    write(
        &root,
        &[
            (
                "main.tex",
                "\\documentclass{book}\n\\begin{document}\n\\end{document}\n",
            ),
            ("chapters/one.tex", "A\n"),
            ("chapters/two.tex", ""),
            ("img/plot.png", ""),
            ("refs.bib", ""),
            ("notes.txt", ""),
        ],
    );
    let state = window(&root);
    let editor = open(&state, &root.join("chapters/one.tex"));
    let has_drop_target = editor
        .view()
        .observe_controllers()
        .iter::<glib::Object>()
        .flatten()
        .any(|c| c.is::<gtk::DropTarget>());
    assert!(has_drop_target, "the view takes file drops");
    let files: Vec<PathBuf> = ["img/plot.png", "chapters/two.tex", "refs.bib", "notes.txt"]
        .iter()
        .map(|f| root.join(f))
        .collect();
    assert!(paste::drop_files(&state, &editor, &files, 2));
    assert_eq!(
        text_of(&editor),
        "A\n\\includegraphics[width=0.8\\textwidth]{../img/plot.png}\n\\input{two}\n\
         \\bibliography{../refs}"
    );
    editor.buffer().undo();
    assert_eq!(text_of(&editor), "A\n");
    assert!(!paste::drop_files(
        &state,
        &editor,
        &[root.join("notes.txt")],
        0
    ));
    // With biblatex in an open document, a .bib file becomes \addbibresource.
    let main = open(&state, &root.join("main.tex"));
    main.buffer().set_text("\\usepackage{biblatex}\n");
    assert!(paste::drop_files(
        &state,
        &editor,
        &[root.join("refs.bib")],
        0
    ));
    assert_eq!(text_of(&editor), "\\addbibresource{../refs.bib}\nA\n");
    close(&state);
}

// ----- templates and CLAUDE.md -----

fn templates_dialog() {
    let root = common::temp_dir("templates");
    let state = window(&root);
    assert!(
        state
            .window
            .lookup_action(templates::TEMPLATE_ACTION)
            .is_some()
    );
    assert!(
        state
            .window
            .lookup_action(templates::INSTRUCTIONS_ACTION)
            .is_some()
    );
    assert!(state.window.lookup_action(bibimport::ACTION).is_some());
    let menu = &state.header.menu_files;
    let actions: Vec<String> = (0..menu.n_items())
        .filter_map(|i| {
            menu.item_attribute_value(i, "action", None)
                .and_then(|v| v.get::<String>())
        })
        .collect();
    for action in [
        "win.new-from-template",
        "win.add-citation",
        "win.create-project-instructions",
    ] {
        assert!(
            actions.iter().any(|a| a == action),
            "{action} in {actions:?}"
        );
    }

    templates::show(&state);
    let d = templates::dialog(&state).expect("the dialog");
    assert_eq!(d.selected().id, "amsart");
    assert!(!d.create_button.is_sensitive(), "no folder yet");
    assert!(d.select("thesis"));
    assert!(!d.select("letter"));
    let target = root.join("thesis");
    d.set_folder(&target);
    assert!(d.create_button.is_sensitive());
    d.create();
    let main = normalize(&target.join("main.tex"));
    common::pump_until(TIMEOUT, || {
        state
            .active_editor()
            .is_some_and(|e| e.document().path() == main)
    });
    assert!(templates::dialog(&state).is_none(), "closed after creating");
    for file in [
        "macros.tex",
        "chapters/introduction.tex",
        "chapters/preliminaries.tex",
        "references.bib",
    ] {
        assert!(target.join(file).is_file(), "{file}");
    }
    assert_eq!(
        state.workspace.root().path(),
        normalize(&target),
        "the new project"
    );

    // Never over existing files: the dialog says so and writes nothing.
    std::fs::write(target.join("macros.tex"), "mine").unwrap();
    templates::show(&state);
    let d = templates::dialog(&state).unwrap();
    d.select("minimal");
    d.set_folder(&target);
    let before = std::fs::read_to_string(&main).unwrap();
    d.create();
    common::pump_until(TIMEOUT, || d.error_text().is_some());
    assert!(
        d.error_text().unwrap().contains("main.tex already exists"),
        "{:?}",
        d.error_text()
    );
    assert_eq!(std::fs::read_to_string(&main).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(target.join("macros.tex")).unwrap(),
        "mine"
    );
    d.dialog.close();
    close(&state);
}

fn templates_files() {
    let root = common::temp_dir("templates-files");
    for t in templates::TEMPLATES {
        let dir = root.join(t.id);
        let main = templates::create(t, &dir).unwrap();
        assert_eq!(main, dir.join("main.tex"));
        for (file, text) in t.files {
            assert_eq!(std::fs::read_to_string(dir.join(file)).unwrap(), *text);
        }
    }
    // All or nothing.
    let dir = root.join("partial");
    write(&dir, &[("references.bib", "x")]);
    let err = templates::create(templates::template("article").unwrap(), &dir).unwrap_err();
    assert!(err.contains("references.bib already exists"), "{err}");
    assert!(!dir.join("main.tex").exists());
    // The thesis's conventions as CLAUDE.md sees them.
    let facts = templates::infer(&root.join("thesis"));
    assert_eq!(facts.main.as_deref(), Some(Path::new("main.tex")));
    assert_eq!(facts.class.as_deref(), Some("book"));
    assert_eq!(facts.bibliography.as_deref(), Some("biblatex with Biber"));
    assert_eq!(facts.macros, [PathBuf::from("macros.tex")]);
    assert_eq!(facts.language.as_deref(), Some("English"));
}

fn instructions() {
    let root = common::temp_dir("instructions");
    write(
        &root,
        &[
            (
                "paper.tex",
                "% !TEX program = lualatex\n\\documentclass{amsart}\n\
                 \\usepackage[british]{babel}\n\\input{notation}\n\
                 \\begin{document}\n\\cite{x}\n\\bibliographystyle{amsalpha}\n\
                 \\bibliography{lit/refs}\n\\end{document}\n",
            ),
            (
                "notation.tex",
                "\\newcommand{\\A}{\\mathbb{A}}\n\\DeclareMathOperator{\\Spec}{Spec}\n\
                 \\newcommand{\\cO}{\\mathcal{O}}\n",
            ),
            ("lit/refs.bib", "@book{x, title = {X}}\n"),
            ("old/draft.tex", "\\section{Old}\n"),
        ],
    );
    let state = window(&root);
    assert!(block_on(templates::create_project_instructions(&state)).unwrap());
    let path = root.join("CLAUDE.md");
    let text = std::fs::read_to_string(&path).unwrap();
    for expected in [
        "Main document: `paper.tex` (class `amsart`).",
        "Engine: LuaLaTeX, run through latexmk.",
        "- BibTeX, style `amsalpha`.",
        "Entries: `lit/refs.bib`.",
        "Macros are defined in `notation.tex`.",
        "written in British English",
        "only through Lemma's tools (`mcp__lemma__edit`, `mcp__lemma__write`)",
    ] {
        assert!(text.contains(expected), "{expected:?} in\n{text}");
    }
    let editor = state.active_editor().expect("CLAUDE.md is open");
    assert_eq!(editor.document().path(), normalize(&path));
    assert_eq!(editor.document().text(), text);

    // Never overwritten: the existing file is opened instead.
    std::fs::write(&path, "my own instructions\n").unwrap();
    close(&state);
    let state = window(&root);
    assert!(!block_on(templates::create_project_instructions(&state)).unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "my own instructions\n"
    );
    assert_eq!(
        state.active_editor().unwrap().document().path(),
        normalize(&path)
    );
    close(&state);
}

// ----- BibTeX import -----

fn citations() {
    let root = common::temp_dir("citations");
    let bib_text = "@book{old, title = {Old}}\n";
    write(&root, &[("main.tex", "See .\n"), ("refs.bib", bib_text)]);
    let state = window(&root);
    let editor = open(&state, &root.join("main.tex"));
    editor.place_cursor_at(4);
    let bib = root.join("refs.bib");

    let added = block_on(bibimport::add_citation(
        &state,
        "https://doi.org/10.2748/tmj/1178244839",
        &bib,
        Some(&editor),
    ))
    .unwrap();
    assert_eq!(added.key, "grothendieck1957quelques");
    assert!(added.appended);
    assert_eq!(text_of(&editor), "See \\cite{grothendieck1957quelques}.\n");
    let on_disk = std::fs::read_to_string(&bib).unwrap();
    assert!(
        on_disk.starts_with(&format!(
            "{bib_text}\n@article{{grothendieck1957quelques,\n"
        )),
        "{on_disk}"
    );
    assert!(
        on_disk.contains("  doi     = {10.2748/tmj/1178244839},\n"),
        "{on_disk}"
    );
    let doc = state
        .workspace
        .get(&normalize(&bib))
        .expect("the .bib document");
    assert_eq!(review_ui::hunk_count(&doc), 1, "one reviewable hunk");

    // Already there: nothing is fetched or appended, the key is reused.
    let requests = REQUESTS.load(Ordering::SeqCst);
    let again = block_on(bibimport::add_citation(
        &state,
        "10.2748/TMJ/1178244839",
        &bib,
        None,
    ))
    .unwrap();
    assert_eq!(again.key, "grothendieck1957quelques");
    assert!(!again.appended);
    assert_eq!(REQUESTS.load(Ordering::SeqCst), requests);

    // arXiv into a second file; a taken key gets a suffix.
    write(
        &root,
        &[("more.bib", "@misc{lovelace2023stacks, title = {T}}")],
    );
    let more = root.join("more.bib");
    let added = block_on(bibimport::add_citation(
        &state,
        "arXiv:2301.01234v2",
        &more,
        None,
    ))
    .unwrap();
    assert_eq!(added.key, "lovelace2023stacksa");
    let text = std::fs::read_to_string(&more).unwrap();
    assert!(text.contains("\n\n@misc{lovelace2023stacksa,\n"), "{text}");
    assert!(text.contains("  eprint        = {2301.01234},\n"), "{text}");
    assert!(text.contains("  primaryclass  = {math.AG},\n"), "{text}");

    // Rejecting the hunk takes the entry out again.
    review_ui::decide_document(&state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || doc.text() == bib_text);

    // A new .bib file is created as a new file under review.
    let fresh = root.join("sub/new.bib");
    std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
    let added = block_on(bibimport::add_citation(&state, "2301.01234", &fresh, None)).unwrap();
    assert!(added.appended);
    assert!(
        std::fs::read_to_string(&fresh)
            .unwrap()
            .starts_with("@misc{lovelace2023stacks,\n")
    );
    let doc = state.workspace.get(&normalize(&fresh)).unwrap();
    assert_eq!(review_ui::hunk_count(&doc), 1);
    close(&state);
}

fn citation_dialog() {
    let root = common::temp_dir("citation-dialog");
    write(
        &root,
        &[
            ("main.tex", "\\bibliography{b}\n"),
            ("a.bib", ""),
            ("b.bib", ""),
        ],
    );
    let state = window(&root);
    let editor = open(&state, &root.join("main.tex"));
    editor.place_cursor_at(0);
    bibimport::show(&state);
    common::pump_until(TIMEOUT, || bibimport::citation_dialog(&state).is_some());
    let d = bibimport::citation_dialog(&state).unwrap();
    assert_eq!(
        d.bib(),
        Some(normalize(&root.join("b.bib"))),
        "the one main.tex names first"
    );
    assert!(d.cite_row.is_active());
    assert!(!d.add_button.is_sensitive());
    d.entry.set_text("not an id");
    assert!(!d.add_button.is_sensitive());
    d.entry.set_text("10.1000/missing");
    assert!(d.add_button.is_sensitive());
    d.add();
    assert!(d.is_busy());
    common::pump_until(TIMEOUT, || !d.is_busy());
    let error = d.error_text().expect("an error");
    assert!(
        error.contains("No record found for DOI 10.1000/missing"),
        "{error}"
    );
    assert!(bibimport::citation_dialog(&state).is_some(), "still open");
    assert_eq!(std::fs::read_to_string(root.join("b.bib")).unwrap(), "");

    d.entry.set_text("2301.01234");
    d.add();
    common::pump_until(TIMEOUT, || bibimport::citation_dialog(&state).is_none());
    assert!(
        std::fs::read_to_string(root.join("b.bib"))
            .unwrap()
            .contains("lovelace2023stacks")
    );
    assert_eq!(
        text_of(&editor),
        "\\cite{lovelace2023stacks}\\bibliography{b}\n"
    );
    close(&state);

    // Without a .bib file the dialog offers a new references.bib.
    let root = common::temp_dir("citation-dialog-new");
    write(&root, &[("main.tex", "")]);
    let state = window(&root);
    bibimport::show(&state);
    common::pump_until(TIMEOUT, || bibimport::citation_dialog(&state).is_some());
    let d = bibimport::citation_dialog(&state).unwrap();
    assert_eq!(d.bibs, [normalize(&root.join("references.bib"))]);
    assert!(!d.cite_row.is_sensitive(), "no document to cite in");
    d.dialog.close();
    close(&state);
}

// ----- auto-save -----

fn type_text(editor: &Editor, text: &str) {
    let buffer = editor.buffer();
    buffer.insert_at_cursor(text);
}

fn autosave_idle() {
    let root = common::temp_dir("autosave-idle");
    write(&root, &[("main.tex", "a\n")]);
    let state = window_with(&root, |c| c.editor.autosave = Autosave::Idle);
    autosave::set_idle_delay(&state, Duration::from_millis(150));
    let path = root.join("main.tex");
    let editor = open(&state, &path);
    editor.place_cursor_at(0);
    type_text(&editor, "b");
    assert!(autosave::is_scheduled(&state));
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(&path).unwrap() == "ba\n" && !editor.document().is_dirty()
    });

    // A file another program changed is not overwritten.
    let other_path = root.join("other.tex");
    std::fs::write(&other_path, "o\n").unwrap();
    let other = open(&state, &other_path);
    std::fs::write(&other_path, "theirs\n").unwrap();
    type_text(&other, "d");
    common::pump_for(Duration::from_millis(600));
    assert_eq!(std::fs::read_to_string(&other_path).unwrap(), "theirs\n");
    assert!(other.document().is_dirty());
    state.close_editor(&other, true);
    state.select_editor(&editor);

    // During an agent turn nothing is saved; the save follows the turn.
    let agent = state.agent_controller().unwrap().clone();
    agent.set_state(AgentState::Running { turn: 1 });
    type_text(&editor, "c");
    common::pump_for(Duration::from_millis(400));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ba\n");
    assert!(autosave::is_deferred(&state));
    agent.set_state(AgentState::Idle);
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(&path).unwrap() == "bca\n"
    });
    close(&state);
}

fn autosave_focus() {
    let root = common::temp_dir("autosave-focus");
    write(&root, &[("main.tex", "a\n")]);
    let state = window_with(&root, |c| c.editor.autosave = Autosave::Focus);
    autosave::set_idle_delay(&state, Duration::from_millis(100));
    let path = root.join("main.tex");
    let editor = open(&state, &path);
    editor.place_cursor_at(0);
    type_text(&editor, "b");
    assert!(
        !autosave::is_scheduled(&state),
        "focus mode has no idle timer"
    );
    common::pump_for(Duration::from_millis(300));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
    autosave::focus_lost(&state);
    common::pump_until(TIMEOUT, || {
        std::fs::read_to_string(&path).unwrap() == "ba\n"
    });

    // Off: neither changes nor focus changes save.
    let mut config = (*state.config).clone();
    config.editor.autosave = Autosave::Off;
    state.set_config(Rc::new(config));
    type_text(&editor, "c");
    assert!(!autosave::is_scheduled(&state));
    autosave::focus_lost(&state);
    assert_eq!(autosave::save_now(&state), 0);
    common::pump_for(Duration::from_millis(200));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ba\n");
    assert!(editor.document().is_dirty());
    close(&state);
}

// ----- regressions -----

/// The text of every label under `widget`.
fn label_texts(widget: &gtk::Widget) -> Vec<String> {
    let mut out = Vec::new();
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(label) = c.downcast_ref::<gtk::Label>() {
            out.push(label.text().to_string());
        }
        out.extend(label_texts(&c));
        child = c.next_sibling();
    }
    out
}

/// File names (and BibTeX keys) are plain text in the Add Citation toast and the template
/// dialog's folder row, never Pango markup: `&` would make the markup invalid and blank them.
fn regress5_markup() {
    let root = common::temp_dir("regress5-markup");
    write(
        &root,
        &[("main.tex", "\\bibliography{r&d}\n"), ("r&d.bib", "")],
    );
    let state = window(&root);
    let editor = open(&state, &root.join("main.tex"));
    editor.place_cursor_at(0);
    bibimport::show(&state);
    common::pump_until(TIMEOUT, || bibimport::citation_dialog(&state).is_some());
    let d = bibimport::citation_dialog(&state).unwrap();
    assert_eq!(d.bib(), Some(normalize(&root.join("r&d.bib"))));
    d.entry.set_text("2301.01234");
    d.add();
    common::pump_until(TIMEOUT, || bibimport::citation_dialog(&state).is_none());
    let toasts = state.toasts.clone().upcast::<gtk::Widget>();
    let shown = |texts: &[String]| {
        texts
            .iter()
            .any(|t| t == "Added lovelace2023stacks to r&d.bib: review the new entry")
    };
    common::pump_for(Duration::from_millis(200));
    let texts = label_texts(&toasts);
    assert!(shown(&texts), "the toast reads {texts:?}");

    templates::show(&state);
    let t = templates::dialog(&state).expect("the dialog");
    let folder = root.join("R&D <new>");
    t.set_folder(&folder);
    let texts = label_texts(t.dialog.upcast_ref());
    let want = folder.display().to_string();
    assert!(texts.contains(&want), "the folder row reads {texts:?}");
    t.dialog.close();
    close(&state);
}

// ----- final review (M5) -----

/// A drop inside a line goes on the next line, never glued to the text; names LaTeX cannot
/// take (`%`, `#`, braces, backslashes) are refused with a toast.
fn regress5_drops() {
    let root = common::temp_dir("r5-drops");
    write(
        &root,
        &[
            ("main.tex", "First line.\nSecond line.\n"),
            ("plot.png", ""),
            ("report 50%.png", ""),
            ("a}b.tex", ""),
            ("x#1.bib", ""),
        ],
    );
    let state = window(&root);
    let editor = open(&state, &root.join("main.tex"));
    // At the start of a line: the line's text stays on its own line.
    assert!(paste::drop_files(
        &state,
        &editor,
        &[root.join("plot.png")],
        0
    ));
    assert_eq!(
        text_of(&editor),
        "\\includegraphics[width=0.8\\textwidth]{plot.png}\nFirst line.\nSecond line.\n"
    );
    editor.buffer().undo();
    // Inside a word: on the next line.
    assert!(paste::drop_files(
        &state,
        &editor,
        &[root.join("plot.png")],
        3
    ));
    assert_eq!(
        text_of(&editor),
        "First line.\n\\includegraphics[width=0.8\\textwidth]{plot.png}\nSecond line.\n"
    );
    editor.buffer().undo();
    let unsafe_names: Vec<PathBuf> = ["report 50%.png", "a}b.tex", "x#1.bib"]
        .iter()
        .map(|f| root.join(f))
        .collect();
    assert!(!paste::drop_files(&state, &editor, &unsafe_names, 0));
    assert_eq!(text_of(&editor), "First line.\nSecond line.\n");
    close(&state);
}

/// "New from Template…" into a folder outside the open project opens the new paper as its own
/// project (a new window), not inside the old root where the agent and project tools cannot
/// reach it.
fn regress5_template_elsewhere() {
    let a = common::temp_dir("r5-template-a");
    write(&a, &[("main.tex", "\\documentclass{article}\n")]);
    let b = common::temp_dir("r5-template-b");
    let state = window(&a);
    let _editor = open(&state, &a.join("main.tex"));
    let launched: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
    let sink = launched.clone();
    templates::set_launcher(Some(Box::new(move |main: &Path| {
        sink.borrow_mut().push(main.to_owned());
        Ok(())
    })));
    templates::show(&state);
    let d = templates::dialog(&state).expect("the dialog");
    d.select("minimal");
    d.set_folder(&b);
    d.create();
    common::pump_until(TIMEOUT, || !launched.borrow().is_empty());
    templates::set_launcher(None);
    let launched: Vec<PathBuf> = launched.borrow().iter().map(|p| normalize(p)).collect();
    assert_eq!(launched, [normalize(&b.join("main.tex"))]);
    assert!(b.join("main.tex").is_file());
    assert_eq!(
        state.workspace.root().path(),
        normalize(&a),
        "this window keeps its project"
    );
    assert!(
        state
            .editors()
            .iter()
            .all(|e| e.document().path() != normalize(&b.join("main.tex"))),
        "the new paper is not opened under the old root"
    );
    close(&state);
}
