//! Agent-edit review in the GUI (WP1.2): rendering goldens, the §4.3 actions and chords,
//! auto-advance, undo, the action-bar pool, the Changes panel and the agent file tools.
//!
//! Run offscreen: `scripts/broadway.sh cargo test -p lemma-app --test gtk_review`.
//! `UPDATE_GOLDEN=1` rewrites the golden images in `tests/fixtures/review/golden/`.

mod common;

use adw::prelude::*;
use lemma_agent::{EditorRequest, ToolReply};
use lemma_app::agent_edits;
use lemma_app::document::DocRef;
use lemma_app::editor::review_overlay::Anchor;
use lemma_app::editor::{Editor, envtools};
use lemma_app::review_ui::{self, Decision};
use lemma_app::testmode::{self, review::reply_text};
use lemma_app::window::WindowState;
use lemma_app::workspace::Workspace;
use lemma_config::Config;
use lemma_core::{Origin, ProjectRoot, TextEdit};
use lemma_review::{Hunk, HunkId, ReviewEvent};
use sourceview5::prelude::*;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    // Rejecting a file the agent created moves it to the trash: keep that out of the user's.
    let data = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("lemma-review-data-{}", std::process::id()));
    std::fs::create_dir_all(&data).expect("create the data directory");
    // SAFETY: no other thread exists yet.
    unsafe { std::env::set_var("XDG_DATA_HOME", &data) };
    let mut tests: Vec<common::Test> = vec![
        ("golden_snapshots", golden_snapshots),
        ("accept_keeps_the_file_on_disk", accept_keeps_disk),
        ("reject_restores_the_baseline_and_saves", reject_restores),
        (
            "ctrl_z_after_reject_reopens_the_same_hunk",
            undo_after_reject,
        ),
        (
            "undo_and_redo_of_a_large_agent_edit_keep_it_under_review",
            undo_redo_large_agent_edit,
        ),
        (
            "closed_documents_keep_only_the_records_of_rejects",
            closed_documents_records,
        ),
        ("alt_u_reverts_accept_and_reject", review_undo),
        ("navigation_wraps_across_files", navigation),
        ("decisions_advance_to_the_next_hunk", auto_advance),
        ("bars_stay_pooled_and_off_the_text", bars_pool),
        ("agent_edit_errors", agent_edit_errors),
        ("disk_equals_buffer_before_each_reply", disk_before_reply),
        (
            "replies_quote_the_change_and_feedback",
            replies_and_feedback,
        ),
        ("user_typing_and_hunks", user_typing),
        (
            "decisions_after_a_pending_rename_settle_it_first",
            decisions_after_a_pending_rename,
        ),
        ("whole_file_and_all_files_decisions", bulk_decisions),
        ("rejected_new_file_goes_to_the_trash", new_file_trash),
        ("ex_commands_and_chords", ex_commands_and_chords),
        ("vim_ex_commands_typed", vim_ex_commands_typed),
        ("banner_summary_and_changes_panel", banner_and_panel),
        ("reading_mode_shrinks_bands", reading_mode),
        ("gutter_numbers_follow_the_text", gutter_alignment),
        ("a_pasted_anchor_line_reserves_nothing", pasted_anchor_line),
        (
            "tags_change_only_where_the_hunks_do",
            tags_change_where_hunks_do,
        ),
        (
            "bands_follow_typing_before_the_recompute",
            bands_follow_typing,
        ),
        ("tags_follow_random_edits", tags_follow_random_edits),
        ("emptied_file_draws_above_the_first_line", emptied_file),
        ("binary_apply_as_agent_and_demo", binary_runs),
    ];
    if std::env::args().any(|a| a == "--ignored") {
        tests.push(("perf_review_many_hunks", perf_many_hunks));
        tests.push(("perf_review_5000_hunks", perf_5000_hunks));
    }
    common::run(&tests)
}

const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture_dir() -> PathBuf {
    common::fixtures().join("review")
}

fn review_main() -> String {
    std::fs::read_to_string(fixture_dir().join("main.tex")).expect("the review fixture")
}

fn block_on<F: Future>(f: F) -> F::Output {
    glib::MainContext::default().block_on(f)
}

/// Settings that make rendering deterministic: light style, no blinking, no animations.
fn deterministic_rendering() {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_cursor_blink(false);
        settings.set_gtk_enable_animations(false);
    }
}

struct Fixture {
    state: Rc<WindowState>,
    dir: PathBuf,
}

fn fixture(name: &str, files: &[(&str, &str)]) -> Fixture {
    fixture_sized(name, files, (900, 720))
}

fn fixture_sized(name: &str, files: &[(&str, &str)], size: (i32, i32)) -> Fixture {
    fixture_with(name, files, size, false)
}

fn fixture_with(name: &str, files: &[(&str, &str)], size: (i32, i32), vim: bool) -> Fixture {
    deterministic_rendering();
    let dir = common::temp_dir(name);
    for (file, text) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let mut config = Config::default();
    config.build.on_save = false;
    config.editor.vim = vim;
    let ws = Workspace::new(ProjectRoot::new(&dir).unwrap());
    let state = WindowState::new(&common::application(), Rc::new(config), None, ws, true);
    state.window.set_default_size(size.0, size.1);
    // The editor gets the whole width: no PDF pane (its content belongs to another package).
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

    fn doc(&self, file: &str) -> DocRef {
        self.state
            .workspace
            .get(&self.path(file))
            .expect("document")
    }

    fn hunks(&self, file: &str) -> Vec<Hunk> {
        self.doc(file)
            .review
            .borrow()
            .as_ref()
            .map(|r| r.hunks().to_vec())
            .unwrap_or_default()
    }

    fn ids(&self, file: &str) -> Vec<HunkId> {
        self.hunks(file).iter().map(|h| h.id).collect()
    }

    fn agent(&self, call: u64, req: EditorRequest) -> ToolReply {
        block_on(agent_edits::apply(&self.state.workspace, call, req))
    }

    fn edit(&self, file: &str, old: &str, new: &str) -> ToolReply {
        let reply = self.agent(
            1,
            EditorRequest::Edit {
                path: file.into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all: false,
            },
        );
        assert!(!reply.is_error, "edit failed: {}", reply_text(&reply));
        reply
    }

    fn write(&self, file: &str, content: &str) -> ToolReply {
        let reply = self.agent(
            1,
            EditorRequest::Write {
                path: file.into(),
                content: content.into(),
            },
        );
        assert!(!reply.is_error, "write failed: {}", reply_text(&reply));
        reply
    }

    /// Activates `win.<name>` and runs the main loop until idle work is done.
    fn action(&self, name: &str) {
        WidgetExt::activate_action(&self.state.window, &format!("win.{name}"), None)
            .unwrap_or_else(|e| panic!("win.{name}: {e}"));
        common::pump_for(Duration::from_millis(30));
    }

    fn cursor_line(&self) -> u32 {
        let editor = self.state.active_editor().expect("an active editor");
        let buffer = editor.buffer();
        buffer.iter_at_mark(&buffer.get_insert()).line() as u32
    }

    fn put_cursor(&self, editor: &Editor, line: u32) {
        let buffer = editor.buffer();
        buffer.place_cursor(&buffer.iter_at_line(line as i32).unwrap());
    }

    fn wait_saved(&self, file: &str) {
        let doc = self.doc(file);
        common::pump_until(TIMEOUT, || {
            std::fs::read_to_string(self.path(file)).ok() == Some(doc.text())
        });
    }

    fn frames(&self, n: u32) {
        assert!(block_on(testmode::wait_for_frames(&self.state.window, n)));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.window.destroy();
    }
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

// ----- goldens -----

/// Each golden case: script name, window width, reading mode.
const GOLDEN_CASES: &[(&str, i32, bool)] = &[
    ("insertion", 900, false),
    ("deletion_mid", 900, false),
    ("eof_deletion", 900, false),
    ("line1", 900, false),
    ("word_change", 900, false),
    ("wrapped", 640, false),
    ("reading_mode", 900, true),
];
/// Golden images are the top of the editor (gutter and text), this tall.
const GOLDEN_HEIGHT: i32 = 540;
/// Largest mean absolute difference per channel (ARCHITECTURE §10.1).
const GOLDEN_TOLERANCE: f64 = 2.0 / 255.0;

fn golden_snapshots() {
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut failures = Vec::new();
    for &(case, width, reading) in GOLDEN_CASES {
        let f = fixture_sized(
            &format!("golden-{case}"),
            &[("main.tex", &review_main())],
            (width, 760),
        );
        let editor = f.open("main.tex");
        std::fs::copy(
            fixture_dir().join(format!("{case}.json")),
            f.path("case.json"),
        )
        .unwrap();
        block_on(testmode::review::run_apply_as_agent(
            f.state.clone(),
            f.path("case.json"),
        ))
        .unwrap_or_else(|e| panic!("{case}: {e}"));
        if reading {
            review_ui::set_reading_mode(&f.state, true);
        }
        f.frames(2);
        let shot = f.path(&format!("{case}.png"));
        block_on(testmode::snapshot_next_frame(editor.widget(), &shot)).unwrap();
        let golden = fixture_dir().join("golden").join(format!("{case}.png"));
        if update {
            crop_png(&shot, &golden, GOLDEN_HEIGHT);
            println!("  updated {}", golden.display());
            continue;
        }
        match image_difference(&shot, &golden) {
            Ok(diff) if diff < GOLDEN_TOLERANCE => {
                println!("  {case}: mean difference {:.4}/255", diff * 255.0);
            }
            Ok(diff) => failures.push(format!(
                "{case}: mean difference {:.3}/255 (actual image: {})",
                diff * 255.0,
                shot.display()
            )),
            Err(e) => failures.push(format!("{case}: {e} (actual image: {})", shot.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "golden mismatches:\n{}",
        failures.join("\n")
    );
}

/// RGBA bytes, width, height and stride of a PNG.
fn pixels(path: &Path) -> Result<(glib::Bytes, usize, usize, usize), String> {
    let texture = gtk::gdk::Texture::from_filename(path)
        .map_err(|e| format!("cannot load {}: {e}", path.display()))?;
    let mut downloader = gtk::gdk::TextureDownloader::new(&texture);
    downloader.set_format(gtk::gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();
    Ok((
        bytes,
        texture.width() as usize,
        texture.height() as usize,
        stride,
    ))
}

/// Saves the top `height` pixels of `src` as `dst`.
fn crop_png(src: &Path, dst: &Path, height: i32) {
    let (bytes, width, full_height, stride) = pixels(src).unwrap();
    let height = (height as usize).min(full_height);
    let data = glib::Bytes::from(&bytes[..stride * height]);
    let texture = gtk::gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gtk::gdk::MemoryFormat::R8g8b8a8,
        &data,
        stride,
    );
    texture.save_to_png(dst).unwrap();
}

/// Mean absolute difference per channel (0..1) between the top of `actual` and `golden`.
fn image_difference(actual: &Path, golden: &Path) -> Result<f64, String> {
    let (a, aw, ah, astride) = pixels(actual)?;
    let (g, gw, gh, gstride) = pixels(golden)?;
    if aw != gw || ah < gh {
        return Err(format!("size {aw}x{ah}, golden {gw}x{gh}"));
    }
    let mut sum = 0u64;
    for y in 0..gh {
        let (ra, rg) = (&a[y * astride..][..gw * 4], &g[y * gstride..][..gw * 4]);
        sum += ra
            .iter()
            .zip(rg)
            .map(|(x, y)| u64::from(x.abs_diff(*y)))
            .sum::<u64>();
    }
    Ok(sum as f64 / (gw * gh * 4) as f64 / 255.0)
}

// ----- decisions -----

const WORD_OLD: &str = "The Krull dimension of $A$ equals $d$.";
const WORD_NEW: &str = "The Krull dimension of $A$ is exactly $d$.";

fn accept_keeps_disk() {
    let f = fixture("accept", &[("main.tex", &review_main())]);
    f.open("main.tex");
    f.edit("main.tex", WORD_OLD, WORD_NEW);
    let proposal = f.doc("main.tex").text();
    assert_eq!(f.disk("main.tex"), proposal, "the agent's edit is on disk");
    assert_eq!(f.hunks("main.tex").len(), 1);

    f.action("review-next");
    f.action("review-accept");
    assert!(f.hunks("main.tex").is_empty(), "accepted hunks disappear");
    assert_eq!(f.doc("main.tex").text(), proposal);
    common::pump_for(Duration::from_millis(100));
    assert_eq!(f.disk("main.tex"), proposal, "accept writes nothing");
    assert!(!f.doc("main.tex").is_dirty());
}

fn reject_restores() {
    let original = review_main();
    let f = fixture("reject", &[("main.tex", &original)]);
    f.open("main.tex");
    f.edit("main.tex", WORD_OLD, WORD_NEW);
    f.action("review-next");
    f.action("review-reject");
    assert!(f.hunks("main.tex").is_empty());
    assert_eq!(f.doc("main.tex").text(), original, "the baseline is back");
    f.wait_saved("main.tex");
    assert_eq!(f.disk("main.tex"), original, "and saved");
}

fn undo_after_reject() {
    let f = fixture("undo-reject", &[("main.tex", &review_main())]);
    let editor = f.open("main.tex");
    f.edit("main.tex", WORD_OLD, WORD_NEW);
    let proposal = f.doc("main.tex").text();
    let id = f.ids("main.tex")[0];
    f.action("review-next");
    f.action("review-reject");
    assert!(f.hunks("main.tex").is_empty());

    // Ctrl+Z (the buffer's undo, as Vim `u` and the keybinding run it).
    editor.buffer().undo();
    common::pump_for(Duration::from_millis(80));
    assert_eq!(f.doc("main.tex").text(), proposal);
    assert_eq!(f.ids("main.tex"), vec![id], "the same HunkId comes back");

    // Redo rejects again; undo re-opens again.
    editor.buffer().redo();
    common::pump_for(Duration::from_millis(80));
    assert!(f.hunks("main.tex").is_empty());
    editor.buffer().undo();
    common::pump_for(Duration::from_millis(80));
    assert_eq!(f.ids("main.tex"), vec![id]);
}

/// Ctrl+Z and Ctrl+Shift+Z of one agent edit with 300 replacements: an open buffer reports each
/// as a deletion and an insertion, 600 changes, more than the review records of older turns
/// (512). The undo leaves nothing to review, the redo brings every hunk back, and none of the
/// agent's changes goes into the baseline on the way (64 did: their records had been dropped, so
/// the redo looked like the user's own typing).
fn undo_redo_large_agent_edit() {
    let original: String = (0..300)
        .map(|i| format!("Line {i} uses \\R here.\nPlain line {i}.\n"))
        .collect();
    let f = fixture("undo-redo-large", &[("main.tex", &original)]);
    let editor = f.open("main.tex");
    let reply = f.agent(
        1,
        EditorRequest::Edit {
            path: "main.tex".into(),
            old_string: "\\R".into(),
            new_string: "\\mathbb{R}".into(),
            replace_all: true,
        },
    );
    assert!(!reply.is_error, "edit failed: {}", reply_text(&reply));
    let proposal = f.doc("main.tex").text();
    let ids = f.ids("main.tex");
    assert_eq!(ids.len(), 300);
    let baseline = || {
        f.doc("main.tex")
            .review
            .borrow()
            .as_ref()
            .map(|r| r.baseline().to_owned())
    };

    editor.buffer().undo();
    common::pump_for(Duration::from_millis(80));
    assert_eq!(f.doc("main.tex").text(), original);
    assert!(f.hunks("main.tex").is_empty());
    assert_eq!(baseline().as_deref(), Some(original.as_str()));

    editor.buffer().redo();
    common::pump_for(Duration::from_millis(80));
    assert_eq!(f.doc("main.tex").text(), proposal);
    let folded = baseline().map_or(0, |b| b.matches("mathbb").count());
    assert_eq!(
        (f.hunks("main.tex").len(), folded),
        (300, 0),
        "hunks after the redo, and agent changes in the baseline"
    );
    assert_eq!(f.ids("main.tex"), ids, "every hunk comes back with its id");
}

/// The agent's replace_all of 600 matches in each of three closed files and an open one. The
/// closed files' reviews keep no record of the agent's changes, which no buffer undo can reach
/// (a closed document opens as not undoable), and closing the open file's tab drops the records
/// of its buffer's. Rejecting every file records the rejects, and Alt+U takes them back in every
/// file, each hunk with its id, on disk too; the rejects' records go then.
fn closed_documents_records() {
    let original: String = (0..600)
        .map(|i| format!("Line {i} uses \\R here.\nPlain line {i}.\n"))
        .collect();
    let names = ["a.tex", "b.tex", "c.tex", "open.tex"];
    let files: Vec<(&str, &str)> = names.iter().map(|n| (*n, original.as_str())).collect();
    let f = fixture("closed-records", &files);
    let editor = f.open("open.tex");
    let held = |file: &str| {
        let doc = f.doc(file);
        let review = doc.review.borrow();
        review.as_ref().expect("under review").held_records()
    };
    for file in names {
        let reply = f.agent(
            1,
            EditorRequest::Edit {
                path: file.into(),
                old_string: "\\R".into(),
                new_string: "\\mathbb{R}".into(),
                replace_all: true,
            },
        );
        assert!(!reply.is_error, "edit failed: {}", reply_text(&reply));
        assert_eq!(f.hunks(file).len(), 600, "{file}");
    }
    for file in &names[..3] {
        assert!(!f.doc(file).is_open());
        assert_eq!(held(file), (0, 0), "{file}");
    }
    assert!(held("open.tex").0 >= 600, "{:?}", held("open.tex"));
    f.state.close_editor(&editor, false);
    drop(editor);
    common::pump_until(TIMEOUT, || !f.doc("open.tex").is_open());
    assert_eq!(f.hunks("open.tex").len(), 600);
    assert_eq!(held("open.tex"), (0, 0), "the closed tab's records");

    let ids: Vec<Vec<HunkId>> = names.iter().map(|n| f.ids(n)).collect();
    let proposal = f.doc("a.tex").text();
    f.action("review-reject-all");
    for file in names {
        f.wait_saved(file);
        assert_eq!(f.disk(file), original, "{file}");
        assert_eq!(held(file).0, 600, "{file}: the rejects' records");
    }
    f.action("review-undo");
    for (file, ids) in names.iter().zip(&ids) {
        assert_eq!(f.ids(file), *ids, "{file}: Alt+U brings every hunk back");
        f.wait_saved(file);
        assert_eq!(f.disk(file), proposal, "{file}");
        assert_eq!(held(file), (0, 0), "{file}");
    }
}

fn review_undo() {
    let original = review_main();
    let f = fixture("alt-u", &[("main.tex", &original)]);
    f.open("main.tex");
    f.edit("main.tex", WORD_OLD, WORD_NEW);
    let proposal = f.doc("main.tex").text();
    let id = f.ids("main.tex")[0];

    f.action("review-next");
    f.action("review-accept");
    assert!(f.hunks("main.tex").is_empty());
    f.action("review-undo");
    assert_eq!(
        f.ids("main.tex"),
        vec![id],
        "Alt+U after Alt+Y brings the hunk back"
    );
    assert_eq!(f.cursor_line(), f.hunks("main.tex")[0].cur_lines.start);

    f.action("review-reject");
    assert_eq!(f.doc("main.tex").text(), original);
    f.action("review-undo");
    assert_eq!(
        f.ids("main.tex"),
        vec![id],
        "Alt+U after Alt+N re-applies the proposal"
    );
    assert_eq!(f.doc("main.tex").text(), proposal);
    f.wait_saved("main.tex");

    // Nothing left to revert: a toast, no change.
    f.action("review-undo");
    f.action("review-undo");
    assert_eq!(f.ids("main.tex"), vec![id]);
}

/// Three hunks in a.tex (open), one in b.tex (never opened).
fn two_file_fixture(name: &str) -> Fixture {
    let f = fixture(name, &[("a.tex", &lines(60)), ("b.tex", &lines(20))]);
    f.open("a.tex");
    f.edit("a.tex", "line 5\n", "line five\n");
    f.edit("a.tex", "line 25\n", "line twenty-five\n");
    f.edit("a.tex", "line 45\n", "line forty-five\n");
    f.edit("b.tex", "line 10\n", "line ten\n");
    assert!(!f.doc("b.tex").is_open());
    f
}

fn active_file(f: &Fixture) -> String {
    f.state.active_editor().unwrap().document().display_name()
}

fn navigation() {
    let f = two_file_fixture("navigation");
    let starts: Vec<u32> = f.hunks("a.tex").iter().map(|h| h.cur_lines.start).collect();
    assert_eq!(starts, [4, 24, 44]);
    f.put_cursor(&f.state.active_editor().unwrap(), 0);
    for &line in &starts {
        f.action("review-next");
        assert_eq!((active_file(&f), f.cursor_line()), ("a.tex".into(), line));
    }
    // Past the last hunk: the next file, which opens in a tab.
    f.action("review-next");
    common::pump_until(TIMEOUT, || active_file(&f) == "b.tex");
    common::pump_until(TIMEOUT, || f.cursor_line() == 9);
    assert!(f.doc("b.tex").is_open());
    // And around to the first hunk of the first file.
    f.action("review-next");
    assert_eq!((active_file(&f), f.cursor_line()), ("a.tex".into(), 4));
    // Backwards wraps the other way.
    f.action("review-prev");
    assert_eq!((active_file(&f), f.cursor_line()), ("b.tex".into(), 9));
    f.action("review-prev");
    assert_eq!((active_file(&f), f.cursor_line()), ("a.tex".into(), 44));
    f.action("review-prev");
    assert_eq!(f.cursor_line(), 24);
}

fn auto_advance() {
    let f = two_file_fixture("advance");
    let editor = f.state.active_editor().unwrap();
    f.put_cursor(&editor, 4);
    f.action("review-accept");
    assert_eq!(f.hunks("a.tex").len(), 2);
    assert_eq!(f.cursor_line(), 24, "the cursor moved to the next hunk");
    f.action("review-reject");
    assert_eq!(f.cursor_line(), 44);
    f.action("review-accept");
    // a.tex is done: on to b.tex.
    common::pump_until(TIMEOUT, || active_file(&f) == "b.tex");
    common::pump_until(TIMEOUT, || f.cursor_line() == 9);
    // The view centres the hunk: the cursor line is inside the visible area.
    let editor = f.state.active_editor().unwrap();
    f.frames(2);
    let view = editor.view();
    let visible = view.visible_rect();
    let (y, h) = view.line_yrange(&editor.buffer().iter_at_line(9).unwrap());
    assert!(y >= visible.y() && y + h <= visible.y() + visible.height());

    // No hunk at the cursor: nothing happens.
    f.put_cursor(&editor, 0);
    f.action("review-accept");
    assert_eq!(f.hunks("b.tex").len(), 1);
}

// ----- the bar pool -----

/// 200 hunks over 1200 lines: replacements, deletions and insertions.
fn many_hunks() -> (String, String) {
    let base = lines(1200);
    let mut new = String::new();
    for (i, line) in base.lines().enumerate() {
        if i % 6 != 2 {
            new.push_str(line);
            new.push('\n');
            continue;
        }
        match (i / 6) % 3 {
            0 => new.push_str(&format!("{line} (changed by the agent)\n")),
            1 => {}
            _ => new.push_str(&format!("{line}\ninserted after {line}\n")),
        }
    }
    (base, new)
}

fn bars_pool() {
    let (base, new) = many_hunks();
    let f = fixture("bars", &[("big.tex", &base)]);
    let editor = f.open("big.tex");
    f.write("big.tex", &new);
    assert_eq!(f.hunks("big.tex").len(), 200);
    let view = editor.view().clone();
    let review = view.review_state().expect("review rendering is attached");
    let adjustment = view.vadjustment().unwrap();
    f.frames(1);
    // Whole frames on the main thread (layout, snapshot, render): before-paint → after-paint.
    let clock = f.state.window.frame_clock().unwrap();
    let frame_start = Rc::new(std::cell::Cell::new(None::<Instant>));
    let frames = Rc::new(std::cell::RefCell::new(Vec::new()));
    let start = frame_start.clone();
    let before = clock.connect_before_paint(move |_| start.set(Some(Instant::now())));
    let (start, done) = (frame_start.clone(), frames.clone());
    let after = clock.connect_after_paint(move |_| {
        if let Some(t) = start.take() {
            done.borrow_mut().push(t.elapsed());
        }
    });
    let mut paint_times = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for step in 0..=20 {
        let range = adjustment.upper() - adjustment.page_size();
        adjustment.set_value(range * f64::from(step) / 20.0);
        // Under Broadway a frame takes about a second: check the widget allocations on a few
        // steps only; the placement itself happens in an idle callback.
        let allocated = step % 5 == 0;
        if allocated {
            f.frames(2);
        }
        common::pump_for(Duration::from_millis(30));

        let bar_widgets = descendants(&view)
            .iter()
            .filter(|w| w.has_css_class("lemma-review-bar"))
            .count();
        assert_eq!(bar_widgets, 8, "the view has exactly the pool's 8 bars");
        let bars = review.visible_bars();
        assert!(!bars.is_empty() && bars.len() <= 8, "{} bars", bars.len());
        let visible = view.visible_rect();
        for (id, rect) in &bars {
            seen.insert(*id);
            assert_no_text_under(&view, rect);
            let near = rect.y() + rect.height() > visible.y() - visible.height()
                && rect.y() < visible.y() + 2 * visible.height();
            assert!(near, "bars belong to hunks near the visible area");
            if allocated {
                // GTK placed the widget where the controller asked.
                let placed = review.bar_widgets().into_iter().any(|w| {
                    w.is_visible()
                        && w.compute_bounds(&view).is_some_and(|b| {
                            let (x, y) = view.window_to_buffer_coords(
                                gtk::TextWindowType::Widget,
                                b.x().round() as i32,
                                b.y().round() as i32,
                            );
                            (x - rect.x()).abs() <= 1 && (y - rect.y()).abs() <= 1
                        })
                });
                assert!(placed, "a bar widget is allocated at {rect:?}");
            }
        }
        // The review's share of a frame: painting bands and deleted lines.
        for _ in 0..5 {
            let started = Instant::now();
            let snapshot = gtk::Snapshot::new();
            lemma_app::editor::review_overlay::paint(&view, &snapshot);
            drop(snapshot.to_node());
            paint_times.push(started.elapsed());
        }
    }
    clock.disconnect(before);
    clock.disconnect(after);
    assert!(
        seen.len() > 16,
        "bars were reused for hunks all over the file"
    );
    let build = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let report = |name: &str, mut times: Vec<Duration>| {
        times.sort();
        let p = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
        println!(
            "  {name} ({} samples, {build} build): p50 {:.2?}, p99 {:.2?}, max {:.2?}",
            times.len(),
            p(0.5),
            p(0.99),
            times[times.len() - 1]
        );
    };
    report("review paint while scrolling 200 hunks", paint_times);
    // Budget: 8 ms p99 per frame in a release build on Wayland; under Broadway this is
    // reported, not asserted (the daemon throttles frames and renders differently).
    report("whole frames while scrolling", frames.take());
}

/// Every descendant of `widget` (GtkTextView keeps overlays inside an internal child).
fn descendants(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut child = widget.first_child();
    while let Some(c) = child {
        child = c.next_sibling();
        out.extend(descendants(&c));
        out.push(c);
    }
    out
}

/// Asserts that no line's text overlaps `rect` (buffer coordinates).
fn assert_no_text_under(view: &impl IsA<gtk::TextView>, rect: &gtk::gdk::Rectangle) {
    let view = view.as_ref();
    let (mut iter, _) = view.line_at_y(rect.y());
    loop {
        let (line_y, line_h) = view.line_yrange(&iter);
        if line_y >= rect.y() + rect.height() {
            break;
        }
        let below: i32 = iter
            .tags()
            .iter()
            .filter(|t| t.is_pixels_below_lines_set())
            .map(|t| t.pixels_below_lines())
            .sum();
        let text_top = view.iter_location(&iter).y();
        let text_bottom = line_y + line_h - below;
        assert!(
            rect.y() + rect.height() <= text_top || rect.y() >= text_bottom,
            "bar {rect:?} covers the text of line {} ({text_top}..{text_bottom})",
            iter.line() + 1
        );
        if !iter.forward_line() {
            break;
        }
    }
}

// ----- agent file tools -----

fn agent_edit_errors() {
    let f = fixture(
        "errors",
        &[("main.tex", "alpha\nbeta\nalpha\n"), ("dir/x.tex", "x\n")],
    );
    let edit = |path: &str, old: &str, new: &str| {
        f.agent(
            7,
            EditorRequest::Edit {
                path: path.into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all: false,
            },
        )
    };
    let error = |reply: ToolReply| {
        assert!(reply.is_error, "expected an error: {}", reply_text(&reply));
        reply_text(&reply)
    };
    assert_eq!(
        error(edit("../outside.tex", "a", "b")),
        "path outside project root"
    );
    assert_eq!(
        error(edit("/etc/hostname", "a", "b")),
        "path outside project root"
    );
    assert_eq!(
        error(edit("main.tex", "gamma", "delta")),
        "old_string not found — the user may have edited this region; call read_file"
    );
    assert_eq!(
        error(edit("main.tex", "alpha", "omega")),
        "old_string matches 2 times; add context or set replace_all"
    );
    assert!(error(edit("missing.tex", "a", "b")).contains("does not exist"));
    assert!(error(edit("dir", "a", "b")).contains("directory"));
    assert!(error(edit("main.tex", "", "b")).contains("old_string is empty"));
    assert!(error(edit("main.tex", "beta", "beta")).contains("identical"));
    let write = f.agent(
        8,
        EditorRequest::Write {
            path: "../escape.tex".into(),
            content: "x".into(),
        },
    );
    assert_eq!(error(write), "path outside project root");
    let read = f.agent(
        9,
        EditorRequest::ReadFile {
            path: "../escape.tex".into(),
            offset: None,
            limit: None,
        },
    );
    assert_eq!(error(read), "path outside project root");
    assert!(error(f.agent(10, EditorRequest::Compile)).contains("compile"));
    // Nothing was changed or reviewed, and the file was not kept in the workspace.
    assert_eq!(f.disk("main.tex"), "alpha\nbeta\nalpha\n");
    assert!(f.state.workspace.is_empty());

    // replace_all edits every match as one undo step.
    let reply = f.agent(
        11,
        EditorRequest::Edit {
            path: "main.tex".into(),
            old_string: "alpha".into(),
            new_string: "omega".into(),
            replace_all: true,
        },
    );
    assert!(!reply.is_error);
    assert_eq!(f.disk("main.tex"), "omega\nbeta\nomega\n");
    assert_eq!(f.hunks("main.tex").len(), 2);
}

fn disk_before_reply() {
    let f = fixture(
        "disk",
        &[("open.tex", &lines(30)), ("closed.tex", &lines(30))],
    );
    f.open("open.tex");
    let mut call = 0;
    for file in ["open.tex", "closed.tex"] {
        for n in [3, 17, 29, 3] {
            call += 1;
            let old = format!("line {n}\n");
            let reply = f.agent(
                call,
                EditorRequest::Edit {
                    path: file.into(),
                    old_string: old.clone(),
                    new_string: format!("{old}% note {call}\n"),
                    replace_all: false,
                },
            );
            assert!(!reply.is_error, "{}", reply_text(&reply));
            // No main-loop iteration since the reply: the save happened before it.
            assert_eq!(f.disk(file), f.doc(file).text(), "{file} after call {call}");
        }
    }
    assert!(f.doc("open.tex").is_open() && !f.doc("closed.tex").is_open());
    // A rewrite of an open file and a brand-new file.
    let rewritten = lines(30).replace("line 10\n", "line ten\n");
    f.write("open.tex", &rewritten);
    assert_eq!(f.disk("open.tex"), f.doc("open.tex").text());
    let reply = f.write("chapters/new.tex", "\\section{New}\nText.\n");
    assert_eq!(f.disk("chapters/new.tex"), "\\section{New}\nText.\n");
    assert!(reply_text(&reply).starts_with("Created chapters/new.tex (2 lines)"));
    assert_eq!(f.hunks("chapters/new.tex").len(), 1);
}

fn replies_and_feedback() {
    let f = fixture("replies", &[("main.tex", &lines(20))]);
    f.open("main.tex");
    let reply = f.edit("main.tex", "line 10\n", "line ten\nline ten and a half\n");
    let text = reply_text(&reply);
    assert_eq!(
        text,
        "Edited main.tex: lines 10–11\n     7\tline 7\n     8\tline 8\n     9\tline 9\n    10\tline \
         ten\n    11\tline ten and a half\n    12\tline 11\n    13\tline 12\n    14\tline 13\n"
    );
    let read = f.agent(
        2,
        EditorRequest::ReadFile {
            path: "main.tex".into(),
            offset: Some(10),
            limit: Some(2),
        },
    );
    assert_eq!(
        reply_text(&read),
        "    10\tline ten\n    11\tline ten and a half\n… 10 more lines (read on with offset 12)\n"
    );

    // The user rejects: the next reply says so, once.
    f.action("review-next");
    f.action("review-reject");
    let reply = f.edit("main.tex", "line 2\n", "line two\n");
    let text = reply_text(&reply);
    assert!(
        text.contains("<review_feedback>\nmain.tex: the user rejected lines 10–11; your text \"line ten\\nline ten and a half\\n\" was replaced by \"line 10\\n\"\n</review_feedback>"),
        "{text}"
    );
    let reply = f.edit("main.tex", "line 3\n", "line three\n");
    assert!(!reply_text(&reply).contains("review_feedback"));
    // read_file sees the live buffer, unsaved user text included.
    let editor = f.state.active_editor().unwrap();
    editor
        .buffer()
        .insert(&mut editor.buffer().start_iter(), "% typed\n");
    let read = f.agent(
        5,
        EditorRequest::ReadFile {
            path: "main.tex".into(),
            offset: None,
            limit: Some(1),
        },
    );
    assert!(reply_text(&read).starts_with("     1\t% typed\n"));
}

fn user_typing() {
    let f = fixture("typing", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 10\n", "line ten\n");
    let id = f.ids("main.tex")[0];
    let buffer = editor.buffer();
    // Typing outside the hunk never creates one.
    buffer.insert(&mut buffer.iter_at_line(1).unwrap(), "typed ");
    buffer.insert(&mut buffer.end_iter(), "more\n");
    common::pump_for(Duration::from_millis(100));
    assert_eq!(f.ids("main.tex"), vec![id]);
    // Typing inside marks the hunk as touched; rejecting it discards the typing.
    let mut at = buffer.iter_at_line(9).unwrap();
    at.forward_to_line_end();
    buffer.insert(&mut at, "!");
    common::pump_for(Duration::from_millis(100));
    let hunks = f.hunks("main.tex");
    assert_eq!(hunks.len(), 1);
    assert!(hunks[0].user_touched);
    f.put_cursor(&editor, 9);
    f.action("review-reject");
    assert!(f.doc("main.tex").text().contains("\nline 10\n"));
    assert!(
        f.doc("main.tex")
            .text()
            .starts_with("line 1\ntyped line 2\n")
    );
}

/// Vim's insert mode keeps a user action open, and live environment rename gives the `\end` the
/// name typed into its `\begin` when that action closes. A reject of the hunk that holds the
/// `\end` lets that rename go in first (`Document::settle_user_action`) and works on the text it
/// leaves. The reject was refused before, after the review had recorded it: the hunk stayed
/// without its turn, the agent was told it had been rejected, and Alt+U had lost it. Alt+N
/// (`decide`), a row of the Changes panel (`decide_hunks`) and Reject File (`decide_document`);
/// Alt+U brings each reject back with its turn.
fn decisions_after_a_pending_rename() {
    let text = "\\documentclass{article}\n\\begin{document}\n\\begin{align}\n  a &= b\n\
                \\end{align}\nTail.\n\\end{document}\n";
    let f = fixture("pending-rename", &[("main.tex", text)]);
    let editor = f.open("main.tex");
    let doc = editor.document().clone();
    let buffer = editor.buffer().clone();
    common::pump_until(TIMEOUT, || doc.has_line_states());
    // A turn changes the `\end` line and the line before it: one hunk around the `\end`'s name.
    let turn = review_ui::begin_named_turn(&f.state, "Test turn");
    review_ui::start_review(&doc, false);
    let (old, new) = ("  a &= b\n\\end{align}\n", "  a &= c\n\\end{align}%\n");
    let at = text.find(old).unwrap();
    let edit = TextEdit {
        range: at..at + old.len(),
        text: new.into(),
    };
    review_ui::in_turn(&doc, turn, || {
        doc.apply_edits(&[edit], Origin::Agent { call: 0 })
    })
    .unwrap();
    review_ui::recompute(&doc);
    common::pump_for(Duration::from_millis(30));
    let rejected = || {
        doc.review.borrow_mut().as_mut().map_or(0, |r| {
            r.drain_events()
                .iter()
                .filter(|e| matches!(e, ReviewEvent::Rejected { .. }))
                .count()
        })
    };
    // Types `letter` at the end of the `\begin` name, in a user action left open.
    let type_pending = |letter: &str| {
        let text = doc.text();
        let name = text.find("\\begin{al").unwrap() + "\\begin{".len();
        assert!(editor.place_cursor_at(name + text[name..].find('}').unwrap()));
        buffer.begin_user_action();
        buffer.insert_interactive_at_cursor(letter, true);
        assert!(
            envtools::linked(&editor).is_some_and(|l| l.dirty),
            "the rename waits"
        );
    };
    // The text once a reject has given the hunk its old text back, the `\begin` renamed.
    let rejected_text = |name: &str| text.replace("{align}\n  a", &format!("{{{name}}}\n  a"));

    let hunks = f.hunks("main.tex");
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].turns, [turn]);
    rejected();
    type_pending("x");
    assert_eq!(
        review_ui::decide(&f.state, &doc, hunks[0].id, Decision::Reject),
        Ok(())
    );
    buffer.end_user_action();
    assert_eq!(doc.text(), rejected_text("alignx"));
    assert!(f.hunks("main.tex").is_empty());
    assert_eq!(rejected(), 1);
    // Alt+U brings the hunk back with its turn, and the rename that went into it.
    f.action("review-undo");
    let hunks = f.hunks("main.tex");
    assert_eq!(hunks.len(), 1, "{}", doc.text());
    assert_eq!(hunks[0].turns, [turn]);
    assert!(doc.text().contains("\\end{alignx}%\n"), "{}", doc.text());

    type_pending("y");
    review_ui::decide_hunks(&f.state, &doc, &[hunks[0].id], Decision::Reject);
    buffer.end_user_action();
    assert_eq!(doc.text(), rejected_text("alignxy"));
    assert!(f.hunks("main.tex").is_empty());
    assert_eq!(rejected(), 1);
    f.action("review-undo");
    assert_eq!(f.hunks("main.tex")[0].turns, [turn]);

    type_pending("z");
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    buffer.end_user_action();
    assert_eq!(doc.text(), rejected_text("alignxyz"));
    assert!(f.hunks("main.tex").is_empty());
    assert_eq!(rejected(), 1);
    f.wait_saved("main.tex");
}

fn bulk_decisions() {
    let original_a = lines(40);
    let original_b = lines(10);
    let f = fixture("bulk", &[("a.tex", &original_a), ("b.tex", &original_b)]);
    f.open("a.tex");
    let propose = |f: &Fixture| {
        f.edit("a.tex", "line 3\n", "line three\n");
        f.edit("a.tex", "line 30\n", "");
        f.edit("b.tex", "line 5\n", "line 5\nline 5b\n");
    };
    propose(&f);
    f.action("review-reject-file");
    assert!(f.hunks("a.tex").is_empty());
    assert_eq!(f.hunks("b.tex").len(), 1, "only the active file");
    f.wait_saved("a.tex");
    assert_eq!(f.disk("a.tex"), original_a);

    f.action("review-reject-all");
    assert!(f.hunks("b.tex").is_empty());
    f.wait_saved("b.tex");
    assert_eq!(f.disk("b.tex"), original_b);

    propose(&f);
    let proposal_b = f.doc("b.tex").text();
    f.action("review-accept-all");
    assert!(f.hunks("a.tex").is_empty() && f.hunks("b.tex").is_empty());
    assert_eq!(f.disk("b.tex"), proposal_b);
    let (files, hunks) = review_ui::summary(&f.state.workspace);
    assert_eq!((files, hunks), (0, 0));
}

fn new_file_trash() {
    let f = fixture("trash", &[("main.tex", "x\n")]);
    f.write("notes/new.tex", "fresh\ntext\n");
    let path = f.path("notes/new.tex");
    assert!(path.exists());
    let doc = f.doc("notes/new.tex");
    review_ui::decide_document(&f.state, &doc, Decision::Reject);
    common::pump_until(TIMEOUT, || !path.exists());
    assert!(
        f.state.workspace.get(&path).is_none(),
        "dropped from the workspace"
    );
    let trash = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("Trash/files");
    let trashed = std::fs::read_dir(&trash)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().starts_with("new"));
    assert!(trashed, "the file is in the trash");
}

fn ex_commands_and_chords() {
    let f = fixture("ex", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    for (action, accels) in review_ui::ACCELS {
        let installed: Vec<_> = f
            .state
            .app
            .accels_for_action(action)
            .iter()
            .map(|a| gtk::accelerator_parse(a.as_str()))
            .collect();
        let wanted: Vec<_> = accels.iter().map(|a| gtk::accelerator_parse(*a)).collect();
        assert!(wanted.iter().all(Option::is_some), "{action}: {accels:?}");
        assert_eq!(installed, wanted, "{action}");
    }
    f.edit("main.tex", "line 4\n", "line four\n");
    f.edit("main.tex", "line 14\n", "line fourteen\n");
    assert!(review_ui::ex_command(&f.state, &editor, ":Hnext"));
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.cursor_line(), 3);
    assert!(review_ui::ex_command(&f.state, &editor, "Reject"));
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.hunks("main.tex").len(), 1);
    assert_eq!(f.cursor_line(), 13, "advanced");
    assert!(review_ui::ex_command(&f.state, &editor, ":Hundo"));
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.hunks("main.tex").len(), 2);
    assert!(review_ui::ex_command(&f.state, &editor, ":AgentAcceptAll"));
    common::pump_for(Duration::from_millis(30));
    assert!(f.hunks("main.tex").is_empty());
    assert!(!review_ui::ex_command(&f.state, &editor, ":w"));
    assert!(!review_ui::ex_command(&f.state, &editor, ":s/a/b/"));
}

/// Types `keys` into the active editor's Vim context, as a keyboard would.
fn type_vim_keys(f: &Fixture, keys: &str) {
    use gtk::gdk;
    let editor = f.state.active_editor().unwrap();
    let im = editor.vim().expect("Vim is on");
    let surface = f.state.window.surface().expect("realized window");
    let display = surface.display();
    let keyboard = display
        .default_seat()
        .and_then(|s| s.keyboard())
        .expect("a keyboard device");
    for c in keys.chars() {
        let keyval = match c {
            '\n' => gdk::Key::Return,
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
        for press in [true, false] {
            im.filter_key(
                press,
                &surface,
                &keyboard,
                0,
                key.keycode(),
                state,
                key.group(),
            );
        }
        common::pump_for(Duration::from_millis(2));
    }
}

fn vim_ex_commands_typed() {
    let f = fixture_with("vim-ex", &[("main.tex", &lines(20))], (900, 720), true);
    f.open("main.tex");
    f.edit("main.tex", "line 6\n", "line six\n");
    let id = f.ids("main.tex")[0];
    type_vim_keys(&f, ":Hnext\n");
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.cursor_line(), 5);
    type_vim_keys(&f, ":Reject\n");
    common::pump_for(Duration::from_millis(30));
    assert!(f.hunks("main.tex").is_empty());
    assert!(f.doc("main.tex").text().contains("\nline 6\n"));
    type_vim_keys(&f, ":Hundo\n");
    common::pump_for(Duration::from_millis(30));
    assert_eq!(f.ids("main.tex"), vec![id]);
    // Vim's own undo (`u`) after a reject re-opens the same hunk too.
    type_vim_keys(&f, ":Reject\n");
    common::pump_for(Duration::from_millis(30));
    assert!(f.hunks("main.tex").is_empty());
    type_vim_keys(&f, "u");
    common::pump_for(Duration::from_millis(80));
    assert_eq!(f.ids("main.tex"), vec![id]);
}

fn banner_and_panel() {
    let f = fixture("banner", &[("a.tex", &lines(20)), ("b.tex", &lines(20))]);
    f.open("a.tex");
    let controller = f.state.review.get().unwrap();
    controller.refresh();
    assert!(!f.state.banner.is_revealed());
    assert!(!controller.summary_button().is_visible());

    f.edit("a.tex", "line 2\n", "line two\n");
    f.edit("a.tex", "line 12\n", "");
    f.edit("b.tex", "line 7\n", "line 7\nline 7b\n");
    controller.refresh();
    assert!(f.state.banner.is_revealed());
    assert_eq!(f.state.banner.title(), "Claude changed 2 files · 3 hunks");
    assert!(controller.summary_button().is_visible());
    let panel = controller.changes_panel().expect("the Changes panel");
    assert_eq!(
        panel.rows(),
        vec![
            ("a.tex".to_owned(), "2 changes · +1 −2".to_owned()),
            ("b.tex".to_owned(), "1 change · +1 −0".to_owned()),
        ]
    );
    panel.diff_toggle().set_active(true);
    let diff = panel.diff_text();
    assert!(diff.contains("--- a/a.tex\n+++ b/a.tex\n"), "{diff}");
    assert!(
        diff.contains("-line 12\n") && diff.contains("+line 7b\n"),
        "{diff}"
    );
    panel.diff_toggle().set_active(false);
    assert!(panel.diff_text().is_empty());

    // The banner's Review button opens the Changes page and goes to the first hunk.
    f.state.banner.emit_by_name::<()>("button-clicked", &[]);
    common::pump_for(Duration::from_millis(30));
    assert_eq!(
        f.state.sidebar.stack.visible_child_name().as_deref(),
        Some("changes")
    );
    assert!(!f.state.banner.is_revealed());
    assert_eq!((active_file(&f), f.cursor_line()), ("a.tex".into(), 1));
    if let Ok(dir) = std::env::var("LEMMA_REVIEW_SHOTS") {
        f.frames(2);
        let shot = Path::new(&dir).join("changes-panel.png");
        block_on(testmode::snapshot_next_frame(&f.state.window, &shot)).unwrap();
        panel.diff_toggle().set_active(true);
        f.frames(1);
        let shot = Path::new(&dir).join("changes-diff.png");
        block_on(testmode::snapshot_next_frame(&f.state.window, &shot)).unwrap();
        panel.diff_toggle().set_active(false);
    }

    review_ui::decide_everything(&f.state, Decision::Accept);
    controller.refresh();
    assert!(panel.rows().is_empty());
    assert!(!controller.summary_button().is_visible());
    assert!(!f.state.banner.is_revealed());
}

fn reading_mode() {
    let f = fixture("reading", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\nline 6\nline 7\n", "");
    f.edit("main.tex", "line 15\n", "line fifteen\n");
    let review = editor.view().review_state().unwrap();
    let header = review.header_height();
    let space = |line: i32| -> i32 {
        let iter = editor.buffer().iter_at_line(line).unwrap();
        iter.tags()
            .iter()
            .filter(|t| t.is_pixels_above_lines_set())
            .map(|t| t.pixels_above_lines())
            .sum()
    };
    let blocks = review.blocks();
    assert_eq!(blocks.len(), 2);
    assert!(
        blocks[0].deleted > 2 * header,
        "three deleted lines are drawn"
    );
    assert_eq!(space(4), header + blocks[0].deleted);
    assert_eq!(space(11), header + blocks[1].deleted);
    assert!(f.state.review.get().is_some_and(|c| !c.reading_mode()));

    f.action("review-reading-mode");
    assert!(f.state.review.get().unwrap().reading_mode());
    assert!(review.reading());
    assert_eq!(space(4), header, "bands shrink to their header");
    assert_eq!(space(11), header);
    assert!(review.blocks().iter().all(|b| b.deleted == 0));

    f.action("review-reading-mode");
    assert_eq!(space(4), header + blocks[0].deleted);
}

fn gutter_alignment() {
    let f = fixture("gutter", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\nline 6\n", "");
    f.frames(2);
    let view = editor.view();
    assert!(!view.shows_line_numbers(), "the stock line numbers are off");
    // The anchor line's text starts below the band and the deleted lines.
    let review = view.review_state().unwrap();
    let block = &review.blocks()[0];
    let iter = editor.buffer().iter_at_line(4).unwrap();
    let (line_y, _) = view.line_yrange(&iter);
    let text_y = view.iter_location(&iter).y();
    assert_eq!(text_y - line_y, review.header_height() + block.deleted);
}

/// 3000 one-line hunks in a 9000-line file (an agent changed every third line, as a Format
/// Document or a project-wide replace can): the overlay's sync after each recompute (the plan,
/// the cached layouts of the deleted lines, the tags), after typing and after a line break that
/// moves the hunks, the gutter's marks in a frame, and accepting a hunk.
fn perf_many_hunks() {
    let base = lines(9000);
    let new: String = base
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i % 3 == 1 {
                format!("{line} changed\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    let f = fixture("perf-hunks", &[("big.tex", &base)]);
    let editor = f.open("big.tex");
    let started = Instant::now();
    f.write("big.tex", &new);
    let first = started.elapsed();
    assert_eq!(f.hunks("big.tex").len(), 3000);
    let view = editor.view().clone();
    let review = view.review_state().expect("review rendering is attached");
    let doc = f.doc("big.tex");
    f.frames(1);
    let mut syncs = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        review.sync();
        syncs.push(started.elapsed());
    }
    let mut recomputes = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        review_ui::recompute(&doc);
        recomputes.push(started.elapsed());
    }
    // Typing outside the hunks, and the recompute after the pause (the hunks stay where they
    // are); then a line break, which moves every hunk below it.
    let buffer = editor.buffer();
    let (mut typed, mut broken) = (Vec::new(), Vec::new());
    for _ in 0..5 {
        buffer.insert(&mut buffer.iter_at_line(4500).unwrap(), "x");
        let started = Instant::now();
        review_ui::recompute(&doc);
        typed.push(started.elapsed());
    }
    for _ in 0..3 {
        buffer.insert(&mut buffer.iter_at_line(4500).unwrap(), "\n");
        let started = Instant::now();
        review_ui::recompute(&doc);
        broken.push(started.elapsed());
    }
    assert_eq!(f.hunks("big.tex").len(), 3000);
    // The middle of the file, where every drawn line looks up its marks.
    let adjustment = view.vadjustment().unwrap();
    adjustment.set_value((adjustment.upper() - adjustment.page_size()) / 2.0);
    f.frames(2);
    let gutter = sourceview5::prelude::ViewExt::gutter(&view, gtk::TextWindowType::Left);
    let parent = gutter.parent().expect("the gutter is in the view");
    let mut gutters = Vec::new();
    for _ in 0..20 {
        // Drawn again, not the nodes of the last frame: the gutter and its renderers.
        gutter.queue_draw();
        let mut child = gutter.first_child();
        while let Some(renderer) = child {
            renderer.queue_draw();
            child = renderer.next_sibling();
        }
        let started = Instant::now();
        let snapshot = gtk::Snapshot::new();
        parent.snapshot_child(&gutter, &snapshot);
        drop(snapshot.to_node());
        gutters.push(started.elapsed());
    }
    let median = |mut times: Vec<Duration>| {
        times.sort();
        times[times.len() / 2]
    };
    let (sync, recompute, gutter) = (median(syncs), median(recomputes), median(gutters));
    let (typed, broken) = (median(typed), median(broken));
    // Accepting the last hunk: the plan has one anchor fewer, whose tag leaves the text.
    let context = glib::MainContext::default();
    let drain = || {
        let started = Instant::now();
        while context.pending() {
            context.iteration(false);
        }
        started.elapsed()
    };
    drain();
    let last = *f.ids("big.tex").last().unwrap();
    let started = Instant::now();
    review_ui::decide_hunks(&f.state, &doc, &[last], Decision::Accept);
    let decided = started.elapsed();
    let relaid = drain();
    assert_eq!(f.hunks("big.tex").len(), 2999);
    // Alt+Y and Alt+N on the hunk in the middle (`decide_and_advance`), five of each in turn:
    // every hunk after it keeps its band, and a reject moves no line.
    let (mut middle, mut middle_loop) = (Vec::new(), Vec::new());
    for k in 0..10 {
        let ids = f.ids("big.tex");
        let decision = if k % 2 == 0 {
            Decision::Accept
        } else {
            Decision::Reject
        };
        drain();
        let started = Instant::now();
        review_ui::decide_and_advance(&f.state, &doc, ids[ids.len() / 2], decision);
        middle.push(started.elapsed());
        middle_loop.push(drain());
    }
    assert_eq!(f.hunks("big.tex").len(), 2989);
    // Typing at the end of a hunk's changed line, which changes its changed words, and the
    // recompute after the pause.
    let mut inside = Vec::new();
    for k in 0..5 {
        let line = f.hunks("big.tex")[300 + 400 * k].cur_lines.start;
        let mut at = buffer.iter_at_line(line as i32).unwrap();
        at.forward_to_line_end();
        buffer.insert(&mut at, "y");
        let started = Instant::now();
        review_ui::recompute(&doc);
        inside.push(started.elapsed());
    }
    assert_eq!(f.hunks("big.tex").len(), 2989);
    let spread = |mut times: Vec<Duration>| {
        times.sort();
        (times[times.len() / 2], times[times.len() - 1])
    };
    let ((middle, middle_max), (middle_loop, middle_loop_max)) =
        (spread(middle), spread(middle_loop));
    let (inside, inside_max) = spread(inside);
    println!(
        "    3000 hunks: the agent's write {first:?}; sync median {sync:?}; recompute (with the \
         sync) median {recompute:?}, after typing {typed:?}, after a line break {broken:?}; \
         gutter snapshot median {gutter:?}; accepting the last {decided:?}, then the main loop \
         {relaid:?}"
    );
    println!(
        "    deciding the middle hunk median {middle:?} (max {middle_max:?}), then the main loop \
         {middle_loop:?} (max {middle_loop_max:?}); typing inside a hunk, the recompute median \
         {inside:?} (max {inside_max:?})"
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(sync <= Duration::from_millis(15), "sync {sync:?}");
        assert!(typed <= Duration::from_millis(40), "after typing {typed:?}");
        assert!(gutter <= Duration::from_millis(1), "gutter {gutter:?}");
        assert!(middle <= Duration::from_millis(50), "a decision {middle:?}");
        assert!(
            inside <= Duration::from_millis(40),
            "typing inside {inside:?}"
        );
    }
}

/// `text` with `tag` added to every third line (the second of each three): an agent's proposal
/// in [`perf_5000_hunks`].
fn every_third_changed(text: &str, tag: &str) -> String {
    text.lines()
        .enumerate()
        .map(|(i, line)| {
            if i % 3 == 1 {
                format!("{line} {tag}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

/// 5000 one-line hunks in a 15 000-line file, from agent turns: keystrokes outside the hunks
/// (against the same keystrokes with no review), Accept all in the file, Keep Turn, Reject all
/// and Reject Turn, each with the main loop's work after it. A reject changes the buffer 10 000
/// times, which costs what as many changes cost without a review (the buffer, its highlighting
/// and observers), measured too; the review's part comes on top of that.
fn perf_5000_hunks() {
    let f = fixture("perf-5000", &[("big.tex", &lines(15_000))]);
    let editor = f.open("big.tex");
    let doc = f.doc("big.tex");
    let buffer = editor.buffer();
    let context = glib::MainContext::default();
    let drain = || {
        let started = Instant::now();
        while context.pending() {
            context.iteration(false);
        }
        started.elapsed()
    };
    // One character at a time on an unchanged line in the middle, as typing does, with the main
    // loop's work between keys (less than the 50 ms before a recompute).
    let keys = |count: usize| {
        let mut times = Vec::with_capacity(count);
        for _ in 0..count {
            let mut at = buffer.iter_at_line(7500).unwrap();
            at.forward_to_line_end();
            let started = Instant::now();
            buffer.insert(&mut at, "k");
            times.push(started.elapsed());
            drain();
        }
        common::pump_for(Duration::from_millis(80));
        times.sort();
        let p = |q: f64| times[((times.len() - 1) as f64 * q).round() as usize];
        (p(0.5), p(0.99), times[times.len() - 1])
    };
    f.frames(1);
    let plain = keys(200);
    // A character of every third line replaced, as one `apply_edits`: what a reject of 5000
    // hunks does to the buffer.
    let mut ends = Vec::new();
    let mut at = 0;
    for (i, line) in doc.text().split_inclusive('\n').enumerate() {
        at += line.len();
        if i % 3 == 1 {
            ends.push(at - 1);
        }
    }
    let edits: Vec<TextEdit> = ends
        .iter()
        .rev()
        .map(|&end| TextEdit {
            range: end - 1..end,
            text: "Z".into(),
        })
        .collect();
    drain();
    let started = Instant::now();
    doc.apply_edits(&edits, Origin::User).unwrap();
    let changes = started.elapsed();
    drain();
    let mut turn = 0;
    let mut propose = |f: &Fixture| {
        turn += 1;
        review_ui::on_turn_started(&f.state, turn);
        let started = Instant::now();
        let proposal = every_third_changed(&f.doc("big.tex").text(), &format!("v{turn}"));
        f.write("big.tex", &proposal);
        let written = started.elapsed();
        review_ui::on_turn_ended(&f.state, turn);
        drain();
        assert_eq!(f.hunks("big.tex").len(), 5000);
        (turn, written)
    };
    let decide = |what: &dyn Fn()| {
        drain();
        let started = Instant::now();
        what();
        let decided = started.elapsed();
        (decided, drain())
    };
    let (_, written) = propose(&f);
    f.frames(1);
    let reviewed = keys(200);
    let accept_all = decide(&|| review_ui::decide_document(&f.state, &doc, Decision::Accept));
    assert!(f.hunks("big.tex").is_empty());
    let (kept, _) = propose(&f);
    let keep_turn = decide(&|| {
        review_ui::decide_turn(&f.state, kept, Decision::Accept);
    });
    assert!(f.hunks("big.tex").is_empty());
    propose(&f);
    let reject_all = decide(&|| review_ui::decide_document(&f.state, &doc, Decision::Reject));
    assert!(f.hunks("big.tex").is_empty());
    let (rejected, _) = propose(&f);
    let reject_turn = decide(&|| {
        review_ui::decide_turn(&f.state, rejected, Decision::Reject);
    });
    assert!(f.hunks("big.tex").is_empty());
    println!(
        "    5000 hunks: the agent's write {written:?}; keystrokes p50/p99/max {:?}/{:?}/{:?} \
         (no review: {:?}/{:?}/{:?}); 10 000 buffer changes without a review {changes:?}",
        reviewed.0, reviewed.1, reviewed.2, plain.0, plain.1, plain.2
    );
    println!(
        "    then the main loop: Accept all {:?} + {:?}, Keep Turn {:?} + {:?}, Reject all \
         {:?} + {:?}, Reject Turn {:?} + {:?}",
        accept_all.0,
        accept_all.1,
        keep_turn.0,
        keep_turn.1,
        reject_all.0,
        reject_all.1,
        reject_turn.0,
        reject_turn.1
    );
    if cfg!(debug_assertions) {
        println!("    (debug build: the budget is checked in release)");
    } else {
        assert!(reviewed.1 <= Duration::from_millis(2), "keystroke p99");
        let budget = Duration::from_millis(300);
        for (what, (decided, _), own) in [
            ("Accept all", accept_all, Duration::ZERO),
            ("Keep Turn", keep_turn, Duration::ZERO),
            ("Reject all", reject_all, changes),
            ("Reject Turn", reject_turn, changes),
        ] {
            assert!(decided <= own + budget, "{what} {decided:?}");
        }
    }
}

/// The lines of `editor`'s text that hunks of `file` make green, the space each line reserves
/// above and below its text, and the bytes of changed words, as the review wants them and as
/// the buffer's tags have them: they must be equal after every apply.
fn assert_tags_match_review(f: &Fixture, file: &str, editor: &Editor, what: &str) {
    let doc = f.doc(file);
    let review = editor.view().review_state().unwrap();
    let buffer = editor.buffer();
    let table = buffer.tag_table();
    let (added, word) = (
        table.lookup("lemma-review-added").unwrap(),
        table.lookup("lemma-review-added-word").unwrap(),
    );
    let index = doc.line_index();
    let lines = index.line_count();
    let hunks = f.hunks(file);
    let header = review.header_height();
    let mut want_added = vec![false; lines as usize];
    let mut want_space = vec![(0, 0); lines as usize];
    let mut want_words = Vec::new();
    for h in &hunks {
        for line in h.cur_lines.clone() {
            want_added[line as usize] = true;
        }
        for w in h.words.iter().filter(|w| w.side == lemma_review::Side::New) {
            let base = index.line_start(h.cur_lines.start + w.line);
            want_words.extend(base + w.bytes.start as usize..base + w.bytes.end as usize);
        }
    }
    for block in review.blocks() {
        let space = header + block.deleted;
        match block.anchor {
            Anchor::Above(line) => want_space[line as usize].0 += space,
            Anchor::Below(line) => want_space[line as usize].1 += space,
            Anchor::Top => {}
        }
    }
    // `LineIndex` lines, as the overlay's: GTK also breaks lines at a lone '\r'.
    let starts: Vec<usize> = (0..lines).map(|line| index.line_start(line)).collect();
    drop(index);
    for line in 0..lines {
        let iter = doc.iter_at_byte(starts[line as usize]).unwrap();
        let tags = iter.tags();
        let above = tags
            .iter()
            .filter(|t| t.is_pixels_above_lines_set())
            .map(|t| t.pixels_above_lines())
            .sum::<i32>();
        let below = tags
            .iter()
            .filter(|t| t.is_pixels_below_lines_set())
            .map(|t| t.pixels_below_lines())
            .sum::<i32>();
        assert_eq!(
            (iter.has_tag(&added), (above, below)),
            (want_added[line as usize], want_space[line as usize]),
            "{what}: line {}",
            line + 1
        );
    }
    let mut words = Vec::new();
    let text = doc.text();
    for (offset, _) in text.char_indices() {
        if doc.iter_at_byte(offset).unwrap().has_tag(&word) {
            words.push(offset);
        }
    }
    let want_words: Vec<usize> = want_words
        .into_iter()
        .filter(|&b| text.is_char_boundary(b))
        .collect();
    assert_eq!(words, want_words, "{what}: changed words");
}

/// Number of tags in `buffer`'s table that reserve space above or below lines.
fn spacing_tags(buffer: &sourceview5::Buffer) -> usize {
    let mut n = 0;
    buffer.tag_table().foreach(|tag| {
        if tag.is_pixels_above_lines_set() || tag.is_pixels_below_lines_set() {
            n += 1;
        }
    });
    n
}

/// Anchors that reserve the same space share a tag, and a decision, typing in a hunk and the
/// recompute after it change the tags of the lines they touch only: counted as the tag
/// operations on the buffer, with the tags checked against the hunks after each.
fn tags_change_where_hunks_do() {
    // 200 hunks, and a first one that changes words.
    let (base, new) = many_hunks();
    let (base, new) = (format!("{WORD_OLD}\n{base}"), format!("{WORD_NEW}\n{new}"));
    let f = fixture("retag", &[("big.tex", &base)]);
    let editor = f.open("big.tex");
    f.write("big.tex", &new);
    assert_eq!(f.hunks("big.tex").len(), 201);
    assert!(!f.hunks("big.tex")[0].words.is_empty());
    let doc = f.doc("big.tex");
    let buffer = editor.buffer();
    assert_tags_match_review(&f, "big.tex", &editor, "the agent's write");
    // Replacements and deletions of one line reserve the same space; insertions only a header.
    assert_eq!(spacing_tags(buffer), 2, "a tag per space, not per hunk");
    let ops = Rc::new(std::cell::Cell::new(0));
    let ours = |tag: &gtk::TextTag| {
        tag.name().is_some_and(|n| n.starts_with("lemma-review-"))
            || tag.is_pixels_above_lines_set()
            || tag.is_pixels_below_lines_set()
    };
    let count = ops.clone();
    let applied = buffer.connect_apply_tag(move |_, tag, _, _| {
        if ours(tag) {
            count.set(count.get() + 1);
        }
    });
    let count = ops.clone();
    let removed = buffer.connect_remove_tag(move |_, tag, _, _| {
        if ours(tag) {
            count.set(count.get() + 1);
        }
    });
    let step = |what: &str, change: &dyn Fn(), most: usize| {
        ops.set(0);
        change();
        let n = ops.get();
        assert!(n <= most, "{what}: {n} tag operations");
        assert_tags_match_review(&f, "big.tex", &editor, what);
    };
    // A hunk in the middle: a replacement, a deletion or an insertion of a line.
    let middle = |shape: fn(&Hunk) -> bool| {
        f.hunks("big.tex")
            .into_iter()
            .skip(100)
            .find(shape)
            .unwrap()
            .id
    };
    let replacement: fn(&Hunk) -> bool = |h| !h.cur_lines.is_empty() && !h.old_text.is_empty();
    let decide = |id: HunkId, decision: Decision| {
        review_ui::decide_hunks(&f.state, &doc, &[id], decision);
    };
    // Each decision changes its own lines' tags: the green line and the anchor.
    step(
        "accepting a replacement",
        &|| decide(middle(replacement), Decision::Accept),
        2,
    );
    step(
        "accepting a deletion",
        &|| decide(middle(|h| h.cur_lines.is_empty()), Decision::Accept),
        1,
    );
    step(
        "rejecting a replacement",
        &|| decide(middle(replacement), Decision::Reject),
        2,
    );
    // The inserted line goes, and its tags with it.
    step(
        "rejecting an insertion",
        &|| decide(middle(|h| h.old_text.is_empty()), Decision::Reject),
        2,
    );
    // Typing at the end of a changed line and the recompute.
    step(
        "typing at the end of a hunk",
        &|| {
            let line = f.hunks("big.tex")[60].cur_lines.start;
            let mut at = buffer.iter_at_line(line as i32).unwrap();
            at.forward_to_line_end();
            buffer.insert(&mut at, " typed");
            review_ui::recompute(&doc);
        },
        2,
    );
    // Typing inside a changed word ("exactly") changes it.
    step(
        "typing in a changed word",
        &|| {
            let at = WORD_NEW.find("exactly").unwrap() + 3;
            buffer.insert(&mut buffer.iter_at_offset(at as i32), "ct");
            review_ui::recompute(&doc);
        },
        2,
    );
    // A line break above every hunk but the first moves them, and GTK moves the tags with the
    // text.
    step(
        "a line break above the hunks",
        &|| {
            buffer.insert(&mut buffer.iter_at_line(2).unwrap(), "\n");
            review_ui::recompute(&doc);
        },
        0,
    );
    // Ctrl+Z of the line break, the typing and the rejects brings the hunks back.
    step(
        "undo",
        &|| {
            for _ in 0..5 {
                buffer.undo();
            }
            review_ui::recompute(&doc);
        },
        12,
    );
    assert_eq!(f.hunks("big.tex").len(), 199);
    step(
        "accepting the changed words",
        &|| decide(f.ids("big.tex")[0], Decision::Accept),
        4,
    );
    // Reading mode moves every anchor to the tag of the header's space.
    let anchors = f.hunks("big.tex").len();
    step(
        "reading mode",
        &|| review_ui::set_reading_mode(&f.state, true),
        2 * anchors,
    );
    assert_eq!(spacing_tags(buffer), 1, "one space: the header's");
    step(
        "reading mode off",
        &|| review_ui::set_reading_mode(&f.state, false),
        2 * anchors,
    );
    // A tag added to the table later outranks the anchor tags: the next apply puts them back on
    // top, so nothing else overrides the space they reserve.
    let later = gtk::TextTag::new(None);
    buffer.tag_table().add(&later);
    step(
        "a decision after another tag came",
        &|| decide(middle(replacement), Decision::Accept),
        2,
    );
    buffer.tag_table().foreach(|tag| {
        if tag.is_pixels_above_lines_set() || tag.is_pixels_below_lines_set() {
            assert!(
                tag.priority() > later.priority(),
                "an anchor tag is outranked"
            );
        }
    });
    buffer.tag_table().remove(&later);
    // Accept all: every tag goes from the text, and the anchor tags from the table. The green
    // background goes in one removal, the anchors one by one (removing one where it is not
    // would lay that text out again).
    step(
        "accept all",
        &|| review_ui::decide_document(&f.state, &doc, Decision::Accept),
        anchors + 2,
    );
    assert_eq!(spacing_tags(buffer), 0);
    buffer.disconnect(applied);
    buffer.disconnect(removed);
}

/// Typing moves the bands before the recompute 50 ms later, while the plan's lines are stale:
/// each band is drawn and marked in the gutter where its line reserves the space, also when
/// text typed at the start of a hunk goes in front of its anchor tag.
fn bands_follow_typing() {
    let f = fixture("stale", &[("main.tex", &lines(30))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 4\n", "line four\n");
    f.edit("main.tex", "line 9\nline 10\n", "");
    f.edit(
        "main.tex",
        "line 14\n",
        "line fourteen\nline fourteen and a half\n",
    );
    f.frames(2);
    let view = editor.view().clone();
    let review = view.review_state().unwrap();
    let buffer = editor.buffer();
    let doc = f.doc("main.tex");
    let header = review.header_height();
    // Each hunk's band is at the top of the line whose text is `anchors[i]`, which reserves its
    // space there, and the gutter marks it at that line only.
    let check = |what: &str, anchors: &[&str]| {
        let ids = f.ids("main.tex");
        let blocks = review.blocks();
        let tops = review.band_tops();
        let text = doc.text();
        assert_eq!(ids.len(), anchors.len());
        for (id, anchor) in ids.iter().zip(anchors) {
            let line = text.lines().position(|l| l == *anchor).unwrap() as u32;
            let iter = buffer.iter_at_line(line as i32).unwrap();
            let (y, _) = view.line_yrange(&iter);
            let block = blocks.iter().find(|b| b.id == *id).unwrap();
            assert_eq!(
                view.iter_location(&iter).y() - y,
                header + block.deleted,
                "{what}: the space above {anchor:?}"
            );
            let top = tops.iter().find(|(i, _)| i == id).map(|&(_, top)| top);
            assert_eq!(top, Some(y), "{what}: the band of {anchor:?}");
            assert_eq!(
                review.hunks_at_line(line),
                [*id],
                "{what}: the gutter at {anchor:?}"
            );
        }
        let marked = (0..doc.line_index().line_count())
            .filter(|&l| !review.hunks_at_line(l).is_empty())
            .count();
        assert_eq!(
            marked,
            anchors.len(),
            "{what}: the gutter marks no other line"
        );
    };
    let anchors = ["line four", "line 11", "line fourteen"];
    check("before typing", &anchors);
    // Nothing runs the main loop until the checks are done: the recompute waits 50 ms.
    buffer.insert(&mut buffer.iter_at_line(1).unwrap(), "\n");
    check("a line break above the hunks", &anchors);
    // Enter inside the first hunk's line moves the others.
    let mut at = buffer.iter_at_line(4).unwrap();
    at.forward_chars(7);
    buffer.insert(&mut at, "\n");
    check(
        "a line break inside a hunk",
        &["line fo", "line 11", "line fourteen"],
    );
    // Enter at the start of a hunk: the new line is in the hunk, the tag stays on the text.
    let line = doc
        .text()
        .lines()
        .position(|l| l == "line fourteen")
        .unwrap();
    buffer.insert(&mut buffer.iter_at_line(line as i32).unwrap(), "\n");
    check(
        "a line break at the start of a hunk",
        &["line fo", "line 11", "line fourteen"],
    );
    // After the recompute the plan matches the text again.
    common::pump_for(Duration::from_millis(150));
    f.frames(1);
    let blocks = review.blocks();
    let tops = review.band_tops();
    for block in &blocks {
        let Anchor::Above(line) = block.anchor else {
            panic!("{:?}", block.anchor);
        };
        let iter = buffer.iter_at_line(line as i32).unwrap();
        let (y, _) = view.line_yrange(&iter);
        assert_eq!(view.iter_location(&iter).y() - y, header + block.deleted);
        assert_eq!(
            tops.iter().find(|(i, _)| *i == block.id).map(|t| t.1),
            Some(y)
        );
        assert_eq!(review.hunks_at_line(line), [block.id]);
    }
    // A deletion at the end of the file reserves its space below the last line with text, and
    // keeps it there while the user types after that line (the hunk then has a line of its own
    // until the recompute).
    drop(f);
    let f = fixture("stale-end", &[("main.tex", &lines(8))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 8\n", "");
    f.frames(2);
    let view = editor.view().clone();
    let review = view.review_state().unwrap();
    let buffer = editor.buffer();
    let [block] = &review.blocks()[..] else {
        panic!("one block");
    };
    assert_eq!(block.anchor, Anchor::Below(6));
    let space = review.header_height() + block.deleted;
    let below_line_7 = || {
        let (y, height) = view.line_yrange(&buffer.iter_at_line(6).unwrap());
        y + height - space
    };
    assert_eq!(review.band_tops(), [(block.id, below_line_7())]);
    buffer.insert(&mut buffer.end_iter(), "x");
    assert_eq!(
        review.band_tops(),
        [(block.id, below_line_7())],
        "typing after the last line"
    );
    assert_eq!(review.hunks_at_line(6), [block.id]);
}

/// The space GTK reserves above the line that starts at `iter`, or below it: that of the tag of
/// the highest priority that sets it.
fn reserved_at(iter: &gtk::TextIter, below: bool) -> i32 {
    iter.tags()
        .into_iter()
        .filter(|t| {
            if below {
                t.is_pixels_below_lines_set()
            } else {
                t.is_pixels_above_lines_set()
            }
        })
        .max_by_key(|t| t.priority())
        .map_or(0, |t| {
            if below {
                t.pixels_below_lines()
            } else {
                t.pixels_above_lines()
            }
        })
}

/// An agent's proposal for [`tags_follow_random_edits`]: every 5th line of `text` changes a word,
/// every 11th goes, and a line follows every 13th.
fn mixed_proposal(text: &str, round: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let (body, end) = line
            .strip_suffix('\n')
            .map_or((line, ""), |body| (body, "\n"));
        match (i + round) % 65 {
            k if k % 11 == 4 => {}
            k if k % 5 == 1 => out.push_str(&format!("{body} r{round}é{end}")),
            k if k % 13 == 7 => out.push_str(&format!("{line}nouveau 漢 {i}\n")),
            _ => out.push_str(line),
        }
    }
    out
}

/// Random typing, deletions and line breaks (also across hunks, with non-ASCII text), pastes,
/// decisions, Ctrl+Z and agent edits: after each recompute the tags are exactly what the hunks
/// want (each tag's ranges followed through the edits since the last apply, or read again), and
/// before it every band the overlay draws is on a line that reserves its space.
fn tags_follow_random_edits() {
    let base: String = (1..=200)
        .map(|i| match i % 9 {
            0 => format!("ligne {i} déjà vu\n"),
            4 => format!("行 {i} 漢字かな\n"),
            _ => format!("line {i} plain text\n"),
        })
        .collect();
    let f = fixture("random-tags", &[("big.tex", &base)]);
    let editor = f.open("big.tex");
    let doc = f.doc("big.tex");
    let view = editor.view().clone();
    let buffer = editor.buffer();
    let review = view.review_state().unwrap();
    let mut round = 0;
    f.write("big.tex", &mixed_proposal(&base, round));
    assert!(f.hunks("big.tex").len() > 40);
    assert_tags_match_review(&f, "big.tex", &editor, "the agent's write");
    // xorshift64: the same steps on every run.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n.max(1) as u64) as usize
    };
    // Every band drawn at a line is where that line reserves its space.
    let bands_on_space = |what: &str| {
        let blocks = review.blocks();
        let header = review.header_height();
        for line in 0..doc.line_index().line_count() {
            let ids = review.hunks_at_line(line);
            if ids.is_empty() {
                continue;
            }
            let start = doc.iter_at_byte(doc.line_index().line_start(line)).unwrap();
            for below in [false, true] {
                let drawn: i32 = blocks
                    .iter()
                    .filter(|b| ids.contains(&b.id))
                    .filter(|b| matches!(b.anchor, Anchor::Below(_)) == below)
                    .map(|b| header + b.deleted)
                    .sum();
                if drawn > 0 {
                    let side = if below { "below" } else { "above" };
                    assert_eq!(
                        reserved_at(&start, below),
                        drawn,
                        "{what}: the space {side} line {}",
                        line + 1
                    );
                }
            }
        }
    };
    for step in 0..150 {
        let chars = buffer.char_count() as usize;
        let what = match next(20) {
            0..=6 => {
                let at = next(chars + 1) as i32;
                let text = ["x", "é", "\n", "ab cd", "漢", " mot\n", "\n\n"][next(7)];
                buffer.insert(&mut buffer.iter_at_offset(at), text);
                "typing"
            }
            7..=9 => {
                let a = next(chars) as i32;
                let most = if next(4) == 0 { 60 } else { 6 };
                let b = (a + 1 + next(most) as i32).min(chars as i32);
                buffer.delete(&mut buffer.iter_at_offset(a), &mut buffer.iter_at_offset(b));
                "a deletion"
            }
            10 => {
                let a = next(chars) as i32;
                let b = (a + 1 + next(40) as i32).min(chars as i32);
                let (start, end) = (buffer.iter_at_offset(a), buffer.iter_at_offset(b));
                let mut at = buffer.iter_at_offset(next(chars + 1) as i32);
                buffer.insert_range(&mut at, &start, &end);
                "a paste"
            }
            11..=13 => {
                review_ui::recompute_if_stale(&doc);
                let ids = f.ids("big.tex");
                if ids.is_empty() {
                    continue;
                }
                let decision = if next(2) == 0 {
                    Decision::Accept
                } else {
                    Decision::Reject
                };
                review_ui::decide_hunks(&f.state, &doc, &[ids[next(ids.len())]], decision);
                "a decision"
            }
            14 | 15 => {
                buffer.undo();
                "Ctrl+Z"
            }
            16 => {
                round += 1;
                review_ui::recompute_if_stale(&doc);
                f.write("big.tex", &mixed_proposal(&doc.text(), round));
                "an agent's write"
            }
            _ => "nothing",
        };
        let what = format!("step {step}, {what}");
        bands_on_space(&what);
        // Sometimes the next edit comes before the recompute.
        if next(3) > 0 {
            review_ui::recompute(&doc);
            assert_tags_match_review(&f, "big.tex", &editor, &what);
            bands_on_space(&what);
        }
    }
}

/// A paste copies the tags of the text it copies (GTK does, within a buffer): the copy of a
/// hunk's anchor line must not reserve the hunk's space a second time once the overlay syncs.
fn pasted_anchor_line() {
    let f = fixture("paste", &[("main.tex", &lines(20))]);
    let editor = f.open("main.tex");
    f.edit("main.tex", "line 5\n", "line five\n");
    f.frames(2);
    let view = editor.view().clone();
    let buffer = editor.buffer();
    let reserved = |line: i32| {
        let iter = buffer.iter_at_line(line).unwrap();
        view.iter_location(&iter).y() - view.line_yrange(&iter).0
    };
    let band = reserved(4);
    assert!(band > 0, "the hunk's band and deleted line");
    // As a paste of the anchor line below the hunk (typing there makes no hunk).
    let start = buffer.iter_at_line(4).unwrap();
    let mut end = start;
    end.forward_line();
    buffer.insert_range(&mut buffer.iter_at_line(15).unwrap(), &start, &end);
    assert_eq!(f.hunks("main.tex").len(), 1);
    review_ui::recompute(&f.doc("main.tex"));
    f.frames(2);
    assert_eq!(reserved(4), band, "the hunk keeps its space");
    assert_eq!(reserved(15), 0, "the pasted line reserves none");
    // Nor is it green.
    let added = buffer.tag_table().lookup("lemma-review-added").unwrap();
    assert!(buffer.iter_at_line(4).unwrap().has_tag(&added));
    assert!(!buffer.iter_at_line(15).unwrap().has_tag(&added));
}

fn emptied_file() {
    let f = fixture("emptied", &[("main.tex", "first\nsecond\n")]);
    let editor = f.open("main.tex");
    let view = editor.view();
    let base_margin = view.top_margin();
    f.write("main.tex", "");
    let review = view.review_state().unwrap();
    let blocks = review.blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        blocks[0].anchor,
        lemma_app::editor::review_overlay::Anchor::Top
    );
    let space = review.header_height() + blocks[0].deleted;
    assert_eq!(
        view.top_margin(),
        base_margin + space,
        "the space comes from the top margin"
    );
    f.frames(2);
    common::pump_for(Duration::from_millis(30));
    let bars = review.visible_bars();
    assert_eq!(bars.len(), 1);
    assert!(bars[0].1.y() < 0, "the bar sits above the first line");
    assert_no_text_under(view, &bars[0].1);
    if let Ok(dir) = std::env::var("LEMMA_REVIEW_SHOTS") {
        let shot = Path::new(&dir).join("emptied.png");
        block_on(testmode::snapshot_next_frame(editor.widget(), &shot)).unwrap();
    }
    f.action("review-next");
    f.action("review-reject");
    assert_eq!(f.doc("main.tex").text(), "first\nsecond\n");
    assert_eq!(view.top_margin(), base_margin, "the margin is restored");
}

// ----- the binary -----

fn binary_runs() {
    let dir = common::temp_dir("binary");
    std::fs::write(dir.join("main.tex"), review_main()).unwrap();
    let png = dir.join("shot.png");
    let lemma = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_lemma"))
            .args(args)
            .env("LEMMA_CONFIG", "/dev/null")
            .env("LEMMA_WINDOW_SIZE", "1000x700")
            .output()
            .expect("run lemma");
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        out
    };
    let out = lemma(&[
        dir.join("main.tex").to_str().unwrap(),
        "--apply-as-agent",
        fixture_dir().join("reading_mode.json").to_str().unwrap(),
        "--snapshot",
        png.to_str().unwrap(),
        "--quit-after-ms",
        "1500",
    ]);
    assert!(out.status.success(), "lemma failed: {:?}", out.status);
    let text = std::fs::read_to_string(dir.join("main.tex")).unwrap();
    assert!(text.contains("is exactly $d$") && !text.contains("\\begin{proof}"));
    let (red, green) = red_and_green_pixels(&png);
    println!("  {red} red and {green} green pixels");
    assert!(red > 500 && green > 500, "hunks are drawn");

    let out = lemma(&[
        "--apply-as-agent",
        "/nonexistent.json",
        "--quit-after-ms",
        "200",
    ]);
    assert_eq!(out.status.code(), Some(1));

    let out = lemma(&["--demo", "review", "--quit-after-ms", "1200"]);
    assert!(out.status.success(), "the review demo failed");
}

fn red_and_green_pixels(png: &Path) -> (usize, usize) {
    let (bytes, width, height, stride) = pixels(png).unwrap();
    let (mut red, mut green) = (0, 0);
    for y in 0..height {
        for x in 0..width {
            let p = &bytes[y * stride + x * 4..][..3];
            let (r, g, b) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
            if r > g + 25 && r > b + 25 {
                red += 1;
            } else if g > r + 15 && g > b + 5 {
                green += 1;
            }
        }
    }
    (red, green)
}
