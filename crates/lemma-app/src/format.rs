//! Formatting with latexindent (M5 WP5.3, from M4): "Format Document" and "Format Selection".
//!
//! - **Availability:** when a window opens, `latexindent --version` runs on the jobs thread;
//!   unless latexindent is missing (it does not start), `win.format-document` and
//!   `win.format-selection` are added (and described for the palette). One that starts but
//!   fails (a Perl module missing, say) keeps them: running one shows its error in a toast.
//! - **Running:** the document's text (or the whole lines the selection touches) goes to
//!   latexindent's standard input on the jobs thread (`latexindent -s --cruft=<temp dir>`, in
//!   that temp dir so nothing lands next to the document), killed after [`TIMEOUT`]. A
//!   selection is sent without its common indentation and gets it back.
//! - **Applying:** the result is a line diff against the text, applied **as reviewable hunks**
//!   in a named review turn "Format" (like Replace in Project, Rename Label and quick fixes):
//!   Keep / Reject per hunk or for the turn, and rejecting restores the text exactly. Nothing
//!   happens when the text changed while latexindent ran, or when it is already formatted.
//!
//! Hooks: [`install`] once per window. Test API: [`set_program`], [`detection_done`],
//! [`format_in`], [`run_latexindent`], [`line_edits`].

use crate::editor::{Editor, envtools};
use crate::review_ui;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_core::{Origin, TextEdit};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long latexindent may run.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// The name of the review turn the result is applied in.
pub const TURN_NAME: &str = "Format";

const ACTIONS: &[(&str, &str)] = &[
    ("format-document", "Format Document"),
    ("format-selection", "Format Selection"),
];

/// The program and timeout to use instead of `latexindent` and [`TIMEOUT`] (tests).
static OVERRIDE: Mutex<Option<(PathBuf, Duration)>> = Mutex::new(None);

/// Uses `program` (a fake latexindent) with `timeout` from the next window on; `None` restores
/// `latexindent` and [`TIMEOUT`].
pub fn set_program(program: Option<(PathBuf, Duration)>) {
    *OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = program;
}

fn program() -> (PathBuf, Duration) {
    OVERRIDE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| (PathBuf::from("latexindent"), TIMEOUT))
}

thread_local! {
    /// Windows whose latexindent check has finished, with its result.
    static DETECTED: std::cell::RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `Some(available)` once the latexindent check of `state`'s window has finished.
pub fn detection_done(state: &WindowState) -> Option<bool> {
    DETECTED.with_borrow(|d| {
        d.iter()
            .find(|(w, _)| w.upgrade().is_some_and(|w| w == state.window))
            .map(|(_, ok)| *ok)
    })
}

/// How [`run_in`] says the program could not be started (it is missing).
const NOT_STARTED: &str = "did not start";

/// Checks for latexindent on the jobs thread and adds the actions unless it is missing.
pub fn install(state: &Rc<WindowState>) {
    let (program, timeout) = program();
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let check = gio::spawn_blocking(move || {
            run(
                &program,
                &["--version"],
                "",
                timeout.min(Duration::from_secs(10)),
            )
        })
        .await
        .unwrap_or_else(|_| Err(format!("latexindent {NOT_STARTED}")));
        // Installed but failing (e.g. a Perl module missing): the actions stay, and running
        // one says why it fails. Only a latexindent that does not start is missing.
        let ok = match check {
            Ok(_) => true,
            Err(e) if e.contains(NOT_STARTED) => false,
            Err(e) => {
                tracing::info!("latexindent fails: {e}");
                true
            }
        };
        let Some(state) = weak.upgrade() else {
            return;
        };
        DETECTED.with_borrow_mut(|d| {
            d.retain(|(w, _)| w.upgrade().is_some());
            d.push((state.window.downgrade(), ok));
        });
        if !ok {
            tracing::info!("latexindent is missing: Format Document is hidden");
            return;
        }
        for &(name, label) in ACTIONS {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(&state);
            let selection = name == "format-selection";
            action.connect_activate(move |_, _| {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                let Some(editor) = state.active_editor() else {
                    return;
                };
                glib::spawn_future_local(async move {
                    if let Err(message) = format_in(&state, &editor, selection).await {
                        state.toast(&message);
                    }
                });
            });
            state.window.add_action(&action);
            crate::app::describe_action(
                &format!("win.{name}"),
                label,
                crate::app::ShortcutGroup::Editing,
                None,
            );
        }
    });
}

/// Runs `program args` with `input` on its standard input, in a fresh private temporary
/// directory (latexindent writes `indent.log` there), and returns its standard output; an error
/// when it cannot start, fails or outlives `timeout` (it is killed then).
fn run(program: &Path, args: &[&str], input: &str, timeout: Duration) -> Result<String, String> {
    // A random name, mode 0700 and created anew: never a directory another user made in /tmp.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::Builder::new()
        .prefix("lemma-latexindent-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .map_err(|e| format!("cannot create a temporary directory: {e}"))?;
    run_in(program, args, input, timeout, dir.path())
}

fn run_in(
    program: &Path,
    args: &[&str],
    input: &str,
    timeout: Duration,
    dir: &Path,
) -> Result<String, String> {
    let name = program.display();
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{name} {NOT_STARTED}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let input = input.to_owned();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let errors = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stderr.read_to_end(&mut out);
        out
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} took longer than {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("{name}: {e}")),
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let err = errors.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("{name} failed ({status}): {first}"));
    }
    String::from_utf8(out).map_err(|_| format!("{name} wrote text that is not UTF-8"))
}

/// Formats `input` with latexindent at `program` (on the calling thread).
pub fn run_latexindent(program: &Path, input: &str, timeout: Duration) -> Result<String, String> {
    let cruft = "--cruft=."; // the temporary working directory
    run(program, &["-s", cruft], input, timeout)
}

/// The edits that turn `old` into `new` (descending), line by line, at byte `base` of the text.
pub fn line_edits(old: &str, new: &str, base: usize) -> Vec<TextEdit> {
    let diff = similar::TextDiff::from_lines(old, new);
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();
    let offset = |lines: &[&str], i: usize| -> usize { lines[..i].iter().map(|l| l.len()).sum() };
    let mut edits: Vec<TextEdit> = diff
        .ops()
        .iter()
        .filter(|op| op.tag() != similar::DiffTag::Equal)
        .map(|op| {
            let (o, n) = (op.old_range(), op.new_range());
            TextEdit {
                range: base + offset(&old_lines, o.start)..base + offset(&old_lines, o.end),
                text: new_lines[n].concat(),
            }
        })
        .collect();
    edits.reverse();
    edits
}

/// The white space every non-blank line of `text` starts with (a selection's indentation).
fn common_indent(text: &str) -> &str {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return "";
    };
    let mut indent = envtools::indent_of(first);
    for l in lines {
        let same = indent
            .bytes()
            .zip(l.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        indent = &indent[..same];
    }
    indent
}

/// `text` without `indent` at the start of its lines.
fn dedent(text: &str, indent: &str) -> String {
    text.split_inclusive('\n')
        .map(|l| l.strip_prefix(indent).unwrap_or(l))
        .collect()
}

/// The text latexindent made of `input`, given back the trailing line break `input` has, and
/// for a selection (sent without its common indentation) that indentation.
fn fit(input: &str, output: &str, selection: bool) -> String {
    let mut out = output.to_owned();
    match (input.ends_with('\n'), out.ends_with('\n')) {
        (false, true) => {
            out.pop();
        }
        (true, false) => out.push('\n'),
        _ => {}
    }
    if !selection {
        return out;
    }
    let indent = common_indent(input);
    if indent.is_empty() {
        return out;
    }
    out.split_inclusive('\n')
        .map(|l| {
            if l.trim().is_empty() {
                l.to_owned()
            } else {
                format!("{indent}{l}")
            }
        })
        .collect()
}

/// Formats `editor`'s document (or the lines of its selection) and applies the result as
/// reviewable hunks. `Ok` also when there was nothing to change (a toast says so).
pub async fn format_in(
    state: &Rc<WindowState>,
    editor: &Rc<Editor>,
    selection: bool,
) -> Result<(), String> {
    let doc = editor.document().clone();
    let version = doc.version();
    let text = doc.text();
    let range = if selection {
        let buffer = editor.buffer();
        let (a, b) = buffer
            .selection_bounds()
            .ok_or("Select the lines to format first")?;
        let (a, b) = (doc.byte_offset(&a), doc.byte_offset(&b));
        let start = envtools::line_bounds(&text, a).start;
        // A selection ending at the start of a line does not include that line.
        let last = if b > a && text[..b].ends_with('\n') {
            b - 1
        } else {
            b
        };
        let end = envtools::line_bounds(&text, last).end;
        start..(end + 1).min(text.len())
    } else {
        0..text.len()
    };
    let input = text[range.clone()].to_owned();
    let (program, timeout) = program();
    // A selection goes without its common indentation, which `fit` gives back.
    let sent = if selection {
        dedent(&input, common_indent(&input))
    } else {
        input.clone()
    };
    let output = gio::spawn_blocking(move || run_latexindent(&program, &sent, timeout))
        .await
        .map_err(|_| "latexindent stopped unexpectedly".to_owned())??;
    if doc.version() != version {
        return Err("The text changed while latexindent ran; format again".to_owned());
    }
    let output = fit(&input, &output, selection);
    let edits = line_edits(&input, &output, range.start);
    if edits.is_empty() {
        state.toast("Already formatted");
        return Ok(());
    }
    let first = edits.last().map_or(0, |e| e.range.start);
    let clean = !doc.is_dirty();
    review_ui::start_review(&doc, false);
    let turn = review_ui::begin_named_turn(state, TURN_NAME);
    review_ui::in_turn(&doc, turn, || {
        envtools::without_live_rename(|| doc.apply_edits(&edits, Origin::Agent { call: 0 }))
    })
    .map_err(|e| format!("Could not apply the formatting: {e}"))?;
    review_ui::recompute(&doc);
    let line = doc.line_index().line_of(first);
    review_ui::show_line(state, &doc, line);
    if clean {
        doc.save()
            .await
            .map_err(|e| format!("Formatted, but saving {} failed: {e}", doc.display_name()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_all(text: &str, edits: &[TextEdit]) -> String {
        let mut s = text.to_owned();
        for e in edits {
            s.replace_range(e.range.clone(), &e.text);
        }
        s
    }

    #[test]
    fn line_edit_table() {
        let cases = [
            ("a\nb\nc\n", "a\nB\nc\n"),
            ("a\n  b\nc", "a\nb\nc\n"),
            ("", "x\n"),
            ("x\ny\n", ""),
            ("same\n", "same\n"),
            ("1\n2\n3\n4\n5\n", "1\n2a\n3\n5\n6\n"),
        ];
        for (old, new) in cases {
            let edits = line_edits(old, new, 0);
            assert!(
                edits.windows(2).all(|w| w[1].range.end <= w[0].range.start),
                "{edits:?}"
            );
            assert_eq!(apply_all(old, &edits), new, "{old:?} → {new:?}");
        }
        assert!(line_edits("same\n", "same\n", 0).is_empty());
        let edits = line_edits("b\n", "c\n", 10);
        assert_eq!(edits[0].range, 10..12);
    }

    #[test]
    fn fitting_the_output() {
        assert_eq!(fit("a", "a\n", false), "a");
        assert_eq!(fit("a\n", "a", false), "a\n");
        assert_eq!(fit("  x\n  y\n", "x\n\ny\n", true), "  x\n\n  y\n");
        assert_eq!(fit("  x\n    y\n", "x\n  z\n", true), "  x\n    z\n");
        assert_eq!(common_indent("  a\n\n    b\n  c"), "  ");
        assert_eq!(common_indent("\ta\n  b"), "");
        assert_eq!(dedent("  a\n    b\nc\n", "  "), "a\n  b\nc\n");
        assert_eq!(fit("x\n", "x\n", true), "x\n");
    }

    #[test]
    fn running_a_program() {
        let sh = Path::new("/bin/sh");
        assert_eq!(
            run(sh, &["-c", "tr a-z A-Z"], "abc\n", TIMEOUT).as_deref(),
            Ok("ABC\n")
        );
        let err = run(sh, &["-c", "echo oops >&2; exit 3"], "", TIMEOUT).unwrap_err();
        assert!(err.contains("oops"), "{err}");
        let t = Instant::now();
        let err = run(sh, &["-c", "sleep 5"], "", Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("took longer"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(3));
        assert!(run(Path::new("/nonexistent/latexindent"), &[], "", TIMEOUT).is_err());
    }

    /// latexindent writes `indent.log` in its working directory: that must be a fresh private
    /// directory, never one another user could have made (or planted a symlink in) under /tmp.
    #[test]
    fn regress5_format_private_tempdir() {
        let planted: Vec<_> = (0..64)
            .map(|n| {
                std::env::temp_dir().join(format!("lemma-latexindent-{}-{n:x}", std::process::id()))
            })
            .filter(|d| std::fs::create_dir(d).is_ok())
            .collect();
        for d in &planted {
            std::fs::write(d.join("planted"), "").unwrap();
        }
        let out = run(
            Path::new("/bin/sh"),
            &["-c", "ls -A; stat -c %a ."],
            "",
            TIMEOUT,
        );
        for d in &planted {
            let _ = std::fs::remove_dir_all(d);
        }
        let out = out.unwrap();
        assert!(
            !out.contains("planted"),
            "ran in a planted directory: {out}"
        );
        assert_eq!(out.trim(), "700", "{out}");
    }
}
