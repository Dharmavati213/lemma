//! Quick actions (WP2.5; QoL P1): prompt templates from `state.prompts()` (the defaults of
//! `data/prompts.default.toml` merged with the user's `prompts.toml`, reloaded live).
//!
//! # Contract
//!
//! - [`install`] (once per window) adds one action `win.quick-<slug>` per template, with the
//!   template's `key` as its default accelerator, and puts them into two menus: the editor's
//!   context menu (`state.editor_menu`, a "Claude" submenu that also holds "Edit with Claude…"
//!   for Ctrl+K) and the panel's "Quick Actions" menu. Templates with `needs_selection` are
//!   disabled while the active editor has no selection. Templates with the `diagnostic`
//!   context ("Fix this error") are not in these menus: they run from the Problems list (or as
//!   `:Agent fix-this-error`, on the build error nearest the cursor). Everything is rebuilt on
//!   `WindowState::connect_config_changed(ConfigFile::Prompts)`.
//! - [`run`] runs a template by name or slug (`lemma_config::find_prompt`), filling it with
//!   `PromptTemplate::fill`: `{selection}` is the selection (or the paragraph at the cursor for
//!   templates that do not need a selection), `{file}` the path relative to the project root,
//!   `{line}` the selection's first line. The prompt goes out with
//!   `PromptOrigin::QuickAction(name)` and the agent pane is shown.
//! - [`agent_command`] is `:Agent <arg>`: a template name or slug runs that quick action, other
//!   text is sent as a prompt, and no text focuses the prompt box.
//! - [`fix_diagnostic`] is the Problems list's "Ask agent to fix": the "Fix this error" template
//!   (or the first `diagnostic` template, or the default one) with the diagnostic's message and
//!   TeX's context, its file and line, and the numbered lines ±20 around it (the live text),
//!   sent with `PromptOrigin::FixDiagnostic`; it shows the agent pane.

use super::{PromptBlock, PromptOrigin, inline, panel};
use crate::diagnostics_panel::Problem;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_config::{PromptContext, PromptTemplate, PromptVars, find_prompt};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// Lines quoted on each side of a diagnostic's line.
pub const CONTEXT_LINES: u32 = 20;
/// The longest selection a quick action sends, in characters; also the most a diagnostic's
/// quoted lines may take.
pub const MAX_SELECTION_CHARS: usize = 50_000;
/// A quoted line of a diagnostic's context longer than this (a generated `\addplot
/// coordinates` line) is cut here, with an ellipsis.
pub const MAX_CONTEXT_LINE_CHARS: usize = 500;
/// The diagnostic's message and TeX context are cut here.
const MAX_DIAGNOSTIC_CHARS: usize = 4_000;

/// Per-window state: the installed actions and the watched buffer.
#[derive(Default)]
struct Quick {
    /// (action name without `win.`, needs a selection).
    actions: RefCell<Vec<(String, bool)>>,
    watch: RefCell<Option<(glib::WeakRef<sourceview5::Buffer>, glib::SignalHandlerId)>>,
}

thread_local! {
    static WINDOWS: RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, Rc<Quick>)>> =
        const { RefCell::new(Vec::new()) };
}

fn quick_of(state: &WindowState) -> Option<Rc<Quick>> {
    WINDOWS.with_borrow_mut(|windows| {
        windows.retain(|(w, _)| w.upgrade().is_some());
        windows
            .iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
            .map(|(_, q)| q.clone())
    })
}

/// Adds the quick actions to the menus (see the module docs).
pub fn install(state: &Rc<WindowState>) {
    let quick = Rc::new(Quick::default());
    WINDOWS.with_borrow_mut(|windows| windows.push((state.window.downgrade(), quick)));
    rebuild(state);
    state.connect_config_changed(|state, file| {
        if file == ConfigFile::Prompts {
            rebuild(state);
        }
    });
    let weak = Rc::downgrade(state);
    state.tab_view.connect_selected_page_notify(move |_| {
        if let Some(state) = weak.upgrade() {
            watch_selection(&state);
        }
    });
    watch_selection(state);
}

/// The action (without `win.`) of each menu template, in menu order.
pub fn actions(state: &WindowState) -> Vec<String> {
    quick_of(state)
        .map(|q| q.actions.borrow().iter().map(|(a, _)| a.clone()).collect())
        .unwrap_or_default()
}

/// `quick-` and the template's slug in ASCII, made unique among `taken`.
fn action_name(template: &PromptTemplate, taken: &[(String, bool)]) -> String {
    let slug: String = template
        .slug()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let base = format!("quick-{}", if slug.is_empty() { "action" } else { &slug });
    let mut name = base.clone();
    let mut n = 2;
    while taken.iter().any(|(a, _)| *a == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

fn rebuild(state: &Rc<WindowState>) {
    let Some(quick) = quick_of(state) else {
        return;
    };
    for (action, _) in quick.actions.take() {
        state
            .app
            .set_accels_for_action(&format!("win.{action}"), &[]);
        state.window.remove_action(&action);
    }
    let templates = state.prompts();
    let items = gio::Menu::new();
    let mut actions = Vec::new();
    for template in templates.iter() {
        if template.context == PromptContext::Diagnostic {
            continue;
        }
        let name = action_name(template, &actions);
        let action = gio::SimpleAction::new(&name, None);
        let (weak, title) = (Rc::downgrade(state), template.name.clone());
        action.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade()
                && let Err(e) = run(&state, &title)
            {
                state.toast(&e);
            }
        });
        state.window.add_action(&action);
        let detailed = format!("win.{name}");
        if let Some(key) = template
            .key
            .as_deref()
            .filter(|k| gtk::accelerator_parse(*k).is_some())
        {
            crate::app::set_default_accels(state, &[(detailed.as_str(), &[key])]);
        }
        // A `[keys]` binding of the action wins over the template's key; the action was just
        // re-created (a reload of prompts.toml), so its binding is put back here.
        match state
            .config
            .keys
            .bindings
            .get(&detailed)
            .map(String::as_str)
        {
            Some("") => state.app.set_accels_for_action(&detailed, &[]),
            Some(accel) if gtk::accelerator_parse(accel).is_some() => {
                state.app.set_accels_for_action(&detailed, &[accel]);
            }
            _ => {}
        }
        items.append(Some(&template.name), Some(&detailed));
        actions.push((name, template.needs_selection));
    }
    quick.actions.replace(actions);

    let claude = gio::Menu::new();
    let edit = gio::Menu::new();
    edit.append(Some("_Edit with Claude…"), Some("win.agent-inline-edit"));
    claude.append_section(None, &edit);
    claude.append_section(None, &items);
    state.editor_menu.remove_all();
    state.editor_menu.append_submenu(Some("_Claude"), &claude);
    if let Some(panel) = panel::get(state) {
        let menu = panel.quick_menu();
        menu.remove_all();
        menu.append_section(None, &items);
    }
    refresh_enabled(state);
}

fn watch_selection(state: &Rc<WindowState>) {
    let Some(quick) = quick_of(state) else {
        return;
    };
    if let Some((buffer, handler)) = quick.watch.take()
        && let Some(buffer) = buffer.upgrade()
    {
        buffer.disconnect(handler);
    }
    if let Some(editor) = state.active_editor() {
        let buffer = editor.buffer().clone();
        let weak = Rc::downgrade(state);
        let handler = buffer.connect_has_selection_notify(move |_| {
            if let Some(state) = weak.upgrade() {
                refresh_enabled(&state);
            }
        });
        quick.watch.replace(Some((buffer.downgrade(), handler)));
    }
    refresh_enabled(state);
}

/// Enables the templates that need a selection only while the active editor has one.
fn refresh_enabled(state: &WindowState) {
    let Some(quick) = quick_of(state) else {
        return;
    };
    let selected = state
        .active_editor()
        .is_some_and(|e| e.buffer().has_selection());
    for (name, needs_selection) in quick.actions.borrow().iter() {
        if let Some(action) = state
            .window
            .lookup_action(name)
            .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
        {
            action.set_enabled(selected || !needs_selection);
        }
    }
}

/// Runs the template named (or slugged) `name` (see the module docs).
pub fn run(state: &Rc<WindowState>, name: &str) -> Result<(), String> {
    let templates = state.prompts();
    let template = find_prompt(&templates, name)
        .ok_or_else(|| format!("There is no quick action “{}”", name.trim()))?
        .clone();
    if template.context == PromptContext::Diagnostic {
        let problem = nearest_problem(state).ok_or("There is no build error to fix")?;
        fix_with(state, &template, &problem);
        return Ok(());
    }
    let editor = state.active_editor().ok_or("No document is open")?;
    let doc = editor.document().clone();
    let buffer = editor.buffer();
    let file = crate::review_ui::relative_name(state, doc.path());
    let (selection, line) = match template.context {
        PromptContext::File => {
            let cursor = buffer.iter_at_mark(&buffer.get_insert());
            (String::new(), cursor.line() + 1)
        }
        _ => {
            let (start, end) = match buffer.selection_bounds() {
                Some(bounds) => bounds,
                None if template.needs_selection => {
                    return Err(format!("“{}” works on a selection", template.name));
                }
                None => inline::paragraph_bounds(&buffer.iter_at_mark(&buffer.get_insert())),
            };
            let text = buffer.text(&start, &end, false).to_string();
            if text.chars().count() > MAX_SELECTION_CHARS {
                return Err(format!(
                    "The selection is too long for a quick action (at most {MAX_SELECTION_CHARS} \
                     characters)"
                ));
            }
            (text, start.line() + 1)
        }
    };
    let text = template.fill(&PromptVars {
        selection: &selection,
        file: &file,
        line: u32::try_from(line).ok(),
        diagnostic: "",
    });
    if panel::send_prompt(
        state,
        vec![PromptBlock::Text(text)],
        PromptOrigin::QuickAction(template.name.clone()),
    ) {
        state.agent.split.set_show_sidebar(true);
        Ok(())
    } else {
        Err(format!(
            "{} is busy",
            super::bridge::agent_label(state.config.agent.backend)
        ))
    }
}

/// `:Agent <arg>` (see the module docs).
pub fn agent_command(state: &Rc<WindowState>, arg: &str) -> Result<(), String> {
    let arg = arg.trim();
    if arg.is_empty() {
        panel::focus_prompt(state);
        return Ok(());
    }
    if find_prompt(&state.prompts(), arg).is_some() {
        return run(state, arg);
    }
    if panel::send_prompt(
        state,
        vec![PromptBlock::Text(arg.to_owned())],
        PromptOrigin::Panel,
    ) {
        state.agent.split.set_show_sidebar(true);
        Ok(())
    } else {
        Err(format!(
            "{} is busy",
            super::bridge::agent_label(state.config.agent.backend)
        ))
    }
}

/// The build problem `:Agent fix-this-error` works on: the active file's problem nearest the
/// cursor, else the first error, else the first problem.
fn nearest_problem(state: &Rc<WindowState>) -> Option<Problem> {
    let diagnostics = state.build.get()?.diagnostics();
    let count = diagnostics.problems().len();
    let problems: Vec<Problem> = (0..count)
        .filter_map(|i| diagnostics.problem_for_agent(i))
        .collect();
    if let Some(editor) = state.active_editor() {
        let (line, _) = editor.cursor_position();
        let here = problems
            .iter()
            .filter(|p| p.path == editor.document().path())
            .filter_map(|p| Some((p.line?.abs_diff(line), p)))
            .min_by_key(|(d, _)| *d)
            .map(|(_, p)| p.clone());
        if here.is_some() {
            return here;
        }
    }
    problems
        .iter()
        .find(|p| p.severity == lemma_core::Severity::Error)
        .or(problems.first())
        .cloned()
}

/// "Ask agent to fix" on `problem`, whose `line` is its line in the current text (see the
/// module docs).
pub fn fix_diagnostic(state: &Rc<WindowState>, problem: &Problem) {
    let templates = state.prompts();
    let template = find_prompt(&templates, "Fix this error")
        .filter(|t| t.context == PromptContext::Diagnostic)
        .or_else(|| {
            templates
                .iter()
                .find(|t| t.context == PromptContext::Diagnostic)
        })
        .cloned()
        .or_else(|| {
            lemma_config::default_prompts()
                .into_iter()
                .find(|t| t.context == PromptContext::Diagnostic)
        });
    let Some(template) = template else {
        state.toast("There is no “Fix this error” quick action");
        return;
    };
    fix_with(state, &template, problem);
}

fn fix_with(state: &Rc<WindowState>, template: &PromptTemplate, problem: &Problem) {
    let send = {
        let (state, template, problem) = (state.clone(), template.clone(), problem.clone());
        move |text: Option<String>| {
            let prompt = diagnostic_prompt(&state, &template, &problem, text.as_deref());
            if panel::send_prompt(
                &state,
                vec![PromptBlock::Text(prompt)],
                PromptOrigin::FixDiagnostic,
            ) {
                state.agent.split.set_show_sidebar(true);
            }
        }
    };
    match state.workspace.get(&problem.path) {
        Some(doc) => send(Some(doc.text())),
        None => {
            let path = problem.path.clone();
            glib::spawn_future_local(async move { send(read_text(&path).await) });
        }
    }
}

/// The filled template for `problem`, with `text` the file's current text.
pub fn diagnostic_prompt(
    state: &WindowState,
    template: &PromptTemplate,
    problem: &Problem,
    text: Option<&str>,
) -> String {
    let file = crate::review_ui::relative_name(state, &problem.path);
    let context = match (text, problem.line) {
        (Some(text), Some(line)) => numbered_context(text, line, CONTEXT_LINES),
        _ => String::new(),
    };
    let message = crate::diagnostics_panel::display_message(&problem.message);
    let diagnostic = match &problem.context {
        Some(tex) => format!("{message}\n{tex}"),
        None => message.to_owned(),
    };
    let diagnostic = truncate_chars(&diagnostic, MAX_DIAGNOSTIC_CHARS);
    template.fill(&PromptVars {
        selection: &context,
        file: &file,
        line: problem.line,
        diagnostic: &diagnostic,
    })
}

/// The lines `line ± around` (1-based) of `text`, numbered, the line itself marked with `>`.
/// A line longer than [`MAX_CONTEXT_LINE_CHARS`] is cut (with `…`), and lines stop before the
/// whole would exceed [`MAX_SELECTION_CHARS`] characters (the marked line is always there).
pub fn numbered_context(text: &str, line: u32, around: u32) -> String {
    let first = line.saturating_sub(around).max(1);
    let last = line.saturating_add(around);
    let mut lines: Vec<(u32, String)> = Vec::new();
    for (i, content) in text.split('\n').enumerate() {
        let n = u32::try_from(i + 1).unwrap_or(u32::MAX);
        if n < first {
            continue;
        }
        if n > last {
            break;
        }
        let marker = if n == line { '>' } else { ' ' };
        let content = content.strip_suffix('\r').unwrap_or(content);
        let content = truncate_chars(content, MAX_CONTEXT_LINE_CHARS);
        lines.push((n, format!("{marker}{n:>5}  {content}\n")));
    }
    // Over the budget (only with an unusual `around`): keep the lines nearest the marked one.
    let mut total: usize = lines.iter().map(|(_, l)| l.chars().count()).sum();
    while total > MAX_SELECTION_CHARS && lines.len() > 1 {
        let far_end = if lines[0].0.abs_diff(line) >= lines[lines.len() - 1].0.abs_diff(line) {
            0
        } else {
            lines.len() - 1
        };
        total -= lines.remove(far_end).1.chars().count();
    }
    lines.into_iter().map(|(_, l)| l).collect()
}

/// `text` cut after `max` characters, with `…` when something was cut.
fn truncate_chars(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    match text.char_indices().nth(max) {
        Some((at, _)) => std::borrow::Cow::Owned(format!("{}…", &text[..at])),
        None => std::borrow::Cow::Borrowed(text),
    }
}

/// Reads `path` as text (lossy UTF-8) without blocking the main thread.
async fn read_text(path: &Path) -> Option<String> {
    let file = gio::File::for_path(path);
    let (bytes, _) = file.load_contents_future().await.ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_context_marks_the_line() {
        let text = "a\nb\nc\nd\ne\n";
        assert_eq!(
            numbered_context(text, 3, 1),
            "     2  b\n>    3  c\n     4  d\n"
        );
        assert_eq!(numbered_context(text, 1, 1), ">    1  a\n     2  b\n");
        assert_eq!(numbered_context("x\r\ny", 2, 5), "     1  x\n>    2  y\n");
        assert_eq!(numbered_context("x", 9, 2), "");
        // A huge generated line is cut; the whole stays bounded.
        let long = "é".repeat(MAX_CONTEXT_LINE_CHARS + 10);
        let text = format!("a\n{long}\nb\n");
        let context = numbered_context(&text, 2, 1);
        let marked = context.lines().nth(1).unwrap();
        assert!(marked.ends_with('…'), "{marked}");
        assert_eq!(
            marked.chars().count(),
            ">    2  ".len() + MAX_CONTEXT_LINE_CHARS + 1
        );
        let many = format!("{long}\n").repeat(400);
        let context = numbered_context(&many, 200, 300);
        assert!(context.chars().count() <= MAX_SELECTION_CHARS);
        assert!(context.contains(">  200  "));
    }

    #[test]
    fn action_names_are_unique_ascii() {
        let mut t = lemma_config::default_prompts()[2].clone();
        assert_eq!(action_name(&t, &[]), "quick-proofread");
        let taken = vec![("quick-proofread".to_owned(), true)];
        assert_eq!(action_name(&t, &taken), "quick-proofread-2");
        t.name = "Übersetzen ins Deutsche".into();
        assert_eq!(action_name(&t, &[]), "quick-bersetzen-ins-deutsche");
        t.name = "ÄÖÜ".into();
        assert_eq!(action_name(&t, &[]), "quick-action");
    }
}
