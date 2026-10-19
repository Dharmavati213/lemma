//! The command palette, and the picker dialog it shares with quick open.
//!
//! # What it shows
//!
//! `win.command-palette` (Ctrl+Shift+P) opens a dialog listing every `win.*` and `app.*` action
//! of the window that takes no parameter ([`commands`]):
//!
//! - the described actions (`app::action_descriptions`: the shell's, M1, M2 and M3 ones, plus
//!   descriptions added at run time with `app::describe_action`), with their label and group;
//! - the quick actions of `prompts.toml` (`win.quick-<slug>`, group Agent);
//! - any other registered action without a parameter, under a label made from its name, so
//!   nothing the user can run is missing.
//!
//! Each row shows the label (the characters the query matched in bold), the group, the current
//! accelerators as keycaps — read from the application's accel registry, so `[keys]` overrides
//! show, alternatives included — the `[vim.normal]` sequences bound to the action while Vim is
//! on, a check mark for switches that are on (Vim Mode, Show Sidebar, …) and "Recent" on
//! recently run commands. An action that is disabled right now is greyed out with the reason
//! (`ActionDescription::unavailable`, "Select some text first" for quick actions that need a
//! selection, else "Not available right now"); choosing it rings the bell.
//!
//! # Filtering and ranking ([`rank`])
//!
//! The query is split into words; every word must match the label (or the action's name,
//! `toggle-vim` → "toggle vim", at a small penalty) fuzzily (frizbee: Smith-Waterman, prefix,
//! word-start and exact-match bonuses; one typo is allowed in words of five or more
//! characters). Matches are ordered by score (recently run commands get a small bonus, enough
//! to win a tie, not to beat a clearly better match), then the shorter label, then registry
//! order. Without a query the recent commands come first (most recent first), then the rest in
//! registry order.
//!
//! The recent commands (at most [`MAX_RECENT`]) are kept per process and, for interactive
//! windows with a state directory, in `$XDG_STATE_HOME/lemma/palette.json` (scripted runs never
//! read or write it; tests use temporary state directories).
//!
//! # The picker
//!
//! Quick open (`quick_open.rs`) uses the same dialog (`Picker`): an `adw::Dialog` (floating
//! over the window, a bottom sheet when narrow) with a search entry over a list. The entry
//! keeps the keyboard: Up/Down, Page Up/Down and Ctrl+N/Ctrl+J move the selection, Enter
//! chooses, Escape closes; a click chooses a row. A new query selects the best match; rows
//! that arrive later (quick open's walk) keep the selection. One picker per window: opening
//! the palette while quick open is up replaces it, also with its key typed in the other picker
//! (the window's accelerators do not reach a dialog, so the picker checks the keys of
//! `win.command-palette` and `win.quick-open` itself); opening the same one again selects its
//! query. When a row is chosen the dialog closes, the focus goes back where it was before the
//! first picker opened (so `win.pdf-search` sees the PDF focused again), and only then the
//! command runs.
//!
//! Hooks: [`show`] is the action; [`run_demo`] is `--demo palette` (resolves when the palette
//! is open with the query "pdf" typed, after one recent command, for `m-palette.png`). Tests
//! use [`commands`], [`rank`], [`vim_accelerator`], [`is_open`], [`dialog`], [`set_query`],
//! [`visible`], [`visible_rows`], [`selected`], [`move_selection`], [`activate_selected`],
//! [`close`], [`recent`], [`recent_for`], [`set_recent`] and [`forget_recent`].

use crate::app::{self, ShortcutGroup};
use crate::window::WindowState;
use adw::prelude::*;
use frizbee::{Config, Matcher, Pattern, Scoring};
use gtk::gdk;
use lemma_config::PromptContext;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How many recently run commands are remembered.
pub const MAX_RECENT: usize = 20;
/// The bonus of the most recently run command when a query is typed; each older one gets
/// [`RECENT_STEP`] less (a match character is worth 12).
const RECENT_BONUS: i32 = 10;
const RECENT_STEP: i32 = 2;
/// A match on the action's name instead of its label scores this much less.
const NAME_PENALTY: i32 = 6;
/// The reason shown for a disabled action that does not say why.
const DEFAULT_REASON: &str = "Not available right now";
/// The reason shown for a disabled quick action that needs a selection.
const SELECTION_REASON: &str = "Select some text first";

// ───────────── Commands ─────────────

/// One command of the palette (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The detailed action name (`win.save`).
    pub name: String,
    pub label: String,
    pub group: ShortcutGroup,
    /// True for a quick action of `prompts.toml`.
    pub quick: bool,
    /// The accelerators bound to it right now (after `[keys]` overrides), GTK syntax.
    pub accels: Vec<String>,
    /// The `[vim.normal]` key sequences that run it (Vim notation).
    pub vim: Vec<String>,
    pub enabled: bool,
    /// Why it is disabled (`None` while enabled).
    pub reason: Option<String>,
    /// The state of a switch (a boolean stateful action), `None` for other actions.
    pub state: Option<bool>,
}

/// The action `name` (`win.x` / `app.x`) as registered on the window or the application.
fn lookup_action(state: &WindowState, name: &str) -> Option<gio::Action> {
    let (prefix, short) = name.split_once('.')?;
    match prefix {
        "win" => state.window.lookup_action(short),
        "app" => state.app.lookup_action(short),
        _ => None,
    }
}

/// "toggle-pdf-sidebar" → "Toggle PDF Sidebar": a label for an action nobody described.
fn label_from_name(short: &str) -> String {
    short
        .split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| match w {
            "pdf" | "Pdf" => "PDF".to_owned(),
            _ => {
                let mut chars = w.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// Every command of `state`'s window, in registry order (see the module docs).
pub fn commands(state: &WindowState) -> Vec<Command> {
    let bindings = state.config.vim.normal_bindings();
    let vim_for = |name: &str| -> Vec<String> {
        if !state.vim_enabled() {
            return Vec::new();
        }
        bindings
            .iter()
            .filter(|(_, action)| *action == name)
            .map(|(keys, _)| keys.clone())
            .collect()
    };
    let accels_for = |name: &str| -> Vec<String> {
        state
            .app
            .accels_for_action(name)
            .iter()
            .map(|a| a.to_string())
            .collect()
    };
    let command = |name: &str, label: String, group: ShortcutGroup, quick, action: &gio::Action| {
        let enabled = action.is_enabled();
        let state_value = action
            .state()
            .filter(|v| v.type_() == glib::VariantTy::BOOLEAN)
            .and_then(|v| v.get::<bool>());
        Command {
            name: name.to_owned(),
            label,
            group,
            quick,
            accels: accels_for(name),
            vim: vim_for(name),
            enabled,
            reason: None,
            state: state_value,
        }
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for d in app::action_descriptions() {
        let Some(action) = lookup_action(state, &d.name) else {
            continue;
        };
        if action.parameter_type().is_some() || !seen.insert(d.name.clone()) {
            continue;
        }
        let mut c = command(&d.name, d.label.clone(), d.group, false, &action);
        if !c.enabled {
            c.reason = Some(d.unavailable.unwrap_or_else(|| DEFAULT_REASON.to_owned()));
        }
        out.push(c);
    }
    // The quick actions: one action per template that is not run from the Problems list, in
    // template order (see `agent::quick`).
    let templates = state.prompts();
    let menu_templates = templates
        .iter()
        .filter(|t| t.context != PromptContext::Diagnostic);
    for (short, template) in crate::agent::quick::actions(state)
        .into_iter()
        .zip(menu_templates)
    {
        let name = format!("win.{short}");
        let Some(action) = lookup_action(state, &name) else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        let mut c = command(
            &name,
            template.name.clone(),
            ShortcutGroup::Agent,
            true,
            &action,
        );
        if !c.enabled {
            c.reason = Some(if template.needs_selection {
                SELECTION_REASON.to_owned()
            } else {
                DEFAULT_REASON.to_owned()
            });
        }
        out.push(c);
    }
    // Anything else the user could run.
    let groups: [(&str, gio::ActionGroup); 2] = [
        ("win", state.window.clone().upcast()),
        ("app", state.app.clone().upcast()),
    ];
    for (prefix, group) in groups {
        let mut names: Vec<String> = group.list_actions().iter().map(|s| s.to_string()).collect();
        names.sort();
        for short in names {
            let name = format!("{prefix}.{short}");
            let Some(action) = lookup_action(state, &name) else {
                continue;
            };
            if action.parameter_type().is_some() || seen.contains(&name) {
                continue;
            }
            seen.insert(name.clone());
            let mut c = command(
                &name,
                label_from_name(&short),
                ShortcutGroup::General,
                false,
                &action,
            );
            if !c.enabled {
                c.reason = Some(DEFAULT_REASON.to_owned());
            }
            out.push(c);
        }
    }
    out
}

// ───────────── Ranking ─────────────

/// A command (or file) that matched a query, best first in [`rank`]'s result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranked {
    /// Its index in the list that was ranked.
    pub index: usize,
    /// The score (frizbee's, plus bonuses); 0 without a query.
    pub score: i32,
    /// Byte offsets of the label's characters the query matched.
    pub positions: Vec<usize>,
}

/// The frizbee matcher of `query`: one fuzzy pattern per word, each of which must match; a
/// word of five or more characters may have one typo. Typing the case of a label earns nothing
/// (frizbee's matching-case bonus would rank "Preferences" above "Find References" for
/// "references"); an upper-case letter in the query still requires it (smart case).
pub(crate) fn matcher(query: &str) -> Option<Matcher> {
    let patterns: Vec<Pattern> = Pattern::parse_query(query.trim())
        .into_iter()
        .map(|p| {
            let typos = u16::from(p.needle.chars().count() >= 5);
            p.max_typos(Some(typos))
        })
        .collect();
    let config = Config {
        scoring: Scoring {
            matching_case_bonus: 0,
            ..Scoring::default()
        },
        ..Config::default()
    };
    (!patterns.is_empty()).then(|| Matcher::from_patterns(&patterns, &config))
}

/// The byte offsets of `matched` (frizbee's indices, bytes of `text`) that start a character.
pub(crate) fn char_positions(text: &str, matched: &[u32]) -> Vec<usize> {
    let mut out: Vec<usize> = matched
        .iter()
        .map(|&i| i as usize)
        .filter(|&i| text.is_char_boundary(i) && i < text.len())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The text the action's name gives for matching: `win.toggle-vim` → "toggle vim".
fn searchable_name(name: &str) -> String {
    name.split_once('.')
        .map_or(name, |(_, short)| short)
        .replace(['-', '_'], " ")
}

/// The palette's order of `commands` for `query`, given the recently run commands (`recent`,
/// action names, most recent first). See the module docs.
pub fn rank(commands: &[Command], query: &str, recent: &[String]) -> Vec<Ranked> {
    let recency = |c: &Command| recent.iter().position(|r| *r == c.name);
    let Some(mut matcher) = matcher(query) else {
        let mut out: Vec<(usize, usize)> = commands
            .iter()
            .enumerate()
            .map(|(i, c)| (recency(c).unwrap_or(usize::MAX), i))
            .collect();
        out.sort();
        return out
            .into_iter()
            .map(|(_, index)| Ranked {
                index,
                score: 0,
                positions: Vec::new(),
            })
            .collect();
    };
    let labels: Vec<&str> = commands.iter().map(|c| c.label.as_str()).collect();
    let names: Vec<String> = commands.iter().map(|c| searchable_name(&c.name)).collect();
    let mut best: Vec<Option<Ranked>> = vec![None; commands.len()];
    for m in matcher.match_list_indices(&labels) {
        let index = m.index as usize;
        best[index] = Some(Ranked {
            index,
            score: i32::from(m.score),
            positions: char_positions(labels[index], &m.indices),
        });
    }
    for m in matcher.match_list(&names) {
        let index = m.index as usize;
        let score = i32::from(m.score) - NAME_PENALTY;
        match &mut best[index] {
            // The label's matched characters stay bold.
            Some(b) => b.score = b.score.max(score),
            None => {
                best[index] = Some(Ranked {
                    index,
                    score,
                    positions: Vec::new(),
                });
            }
        }
    }
    let mut out: Vec<Ranked> = best
        .into_iter()
        .flatten()
        .map(|mut r| {
            if let Some(age) = recency(&commands[r.index]) {
                r.score += (RECENT_BONUS - RECENT_STEP * age as i32).max(0);
            }
            r
        })
        .collect();
    // Equal scores: the shorter label is the closer match ("accept all" → "Accept All
    // Changes" before "Accept All Changes in File").
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(
                commands[a.index]
                    .label
                    .len()
                    .cmp(&commands[b.index].label.len()),
            )
            .then(a.index.cmp(&b.index))
    });
    out
}

// ───────────── Recent commands ─────────────

thread_local! {
    /// The recently run commands, most recent first; `None` until first loaded.
    static RECENT: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// The file the recent commands are kept in, for interactive windows with a state directory.
fn recent_file(state: &WindowState) -> Option<PathBuf> {
    if state.scripted {
        return None;
    }
    state
        .paths
        .as_ref()
        .map(|p| p.state_dir.join("palette.json"))
}

/// The recently run commands, most recent first (read from the state directory the first time).
pub fn recent_for(state: &WindowState) -> Vec<String> {
    if let Some(list) = RECENT.with_borrow(Clone::clone) {
        return list;
    }
    let list: Vec<String> = recent_file(state)
        .and_then(|file| std::fs::read_to_string(file).ok())
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|name| app::is_valid_action_name(name))
        .take(MAX_RECENT)
        .collect();
    RECENT.with_borrow_mut(|r| *r = Some(list.clone()));
    list
}

/// The recently run commands of this process, most recent first.
pub fn recent() -> Vec<String> {
    RECENT.with_borrow(|r| r.clone().unwrap_or_default())
}

/// Forgets the recently run commands of this process, so the file is read again (tests).
pub fn forget_recent() {
    RECENT.with_borrow_mut(|r| *r = None);
}

/// Replaces the recently run commands (tests).
pub fn set_recent(list: Vec<String>) {
    RECENT.with_borrow_mut(|r| *r = Some(list));
}

/// Records that `name` ran: it moves to the front of the recent commands (and the file).
fn remember(state: &WindowState, name: &str) {
    let mut list = recent_for(state);
    list.retain(|n| n != name);
    list.insert(0, name.to_owned());
    list.truncate(MAX_RECENT);
    RECENT.with_borrow_mut(|r| *r = Some(list.clone()));
    if let Some(file) = recent_file(state) {
        glib::spawn_future_local(async move {
            let written = gio::spawn_blocking(move || -> std::io::Result<()> {
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let text = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
                let tmp = file.with_extension(format!("json.{}", std::process::id()));
                std::fs::write(&tmp, text)?;
                std::fs::rename(&tmp, &file)
            })
            .await;
            if let Ok(Err(e)) = written {
                tracing::warn!("could not save the recent commands: {e}");
            }
        });
    }
}

// ───────────── The palette ─────────────

/// The picker kind of the command palette.
const KIND: &str = "palette";

/// Converts Vim notation (`gd`, `]h`, `<Space>a`, `K`, `<C-k>`) into a key sequence that
/// `adw::ShortcutLabel` draws (`g+d`, `bracketright+h`, `space+a`, `<Shift>k`, `<Control>k`);
/// `None` when a key has no keysym.
pub fn vim_accelerator(keys: &str) -> Option<String> {
    let names = crate::editor::keys::parse_keys(keys).ok()?;
    let parts: Option<Vec<String>> = names.iter().map(|n| vim_key_accel(n)).collect();
    Some(parts?.join("+"))
}

/// The keyval of the character `c` (`gdk::Key` has no safe constructor from a keyval number; it
/// is a `u32` GValue).
fn char_key(c: char) -> Option<gdk::Key> {
    gdk::unicode_to_keyval(u32::from(c))
        .to_value()
        .get::<gdk::Key>()
        .ok()
}

/// One canonical Vim key name (`]`, `K`, `<Space>`, `<C-k>`, `<S-Tab>`, `<lt>`) as an accelerator.
fn vim_key_accel(name: &str) -> Option<String> {
    let (mut mods, key) = match name.strip_prefix('<').and_then(|n| n.strip_suffix('>')) {
        Some(inner) if inner.chars().count() > 1 => {
            let mut mods = gdk::ModifierType::empty();
            let mut rest = inner;
            while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
                match rest.as_bytes()[0] {
                    b'C' => mods |= gdk::ModifierType::CONTROL_MASK,
                    b'M' => mods |= gdk::ModifierType::ALT_MASK,
                    b'S' => mods |= gdk::ModifierType::SHIFT_MASK,
                    _ => break,
                }
                rest = &rest[2..];
            }
            let key = match rest {
                "Space" => gdk::Key::space,
                "Tab" => gdk::Key::Tab,
                "CR" => gdk::Key::Return,
                "Esc" => gdk::Key::Escape,
                "BS" => gdk::Key::BackSpace,
                "Del" => gdk::Key::Delete,
                "PageUp" => gdk::Key::Page_Up,
                "PageDown" => gdk::Key::Page_Down,
                "lt" => gdk::Key::less,
                "Bslash" => gdk::Key::backslash,
                "Bar" => gdk::Key::bar,
                other => {
                    let mut chars = other.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => char_key(c)?,
                        _ => gdk::Key::from_name(other)?,
                    }
                }
            };
            (mods, key)
        }
        _ => {
            let c = name.chars().next()?;
            (gdk::ModifierType::empty(), char_key(c)?)
        }
    };
    let mut key = key;
    // `K` is Shift+k for the reader.
    if let Some(c) = key.to_unicode()
        && c.is_uppercase()
        && c.is_alphabetic()
    {
        key = key.to_lower();
        mods |= gdk::ModifierType::SHIFT_MASK;
    }
    if key == gdk::Key::VoidSymbol || key.name().is_none() {
        return None;
    }
    Some(gtk::accelerator_name(key, mods).to_string())
}

/// The picker rows of `commands` for `query`.
fn command_rows(commands: &[Command], query: &str, recent: &[String]) -> Vec<PickerRow> {
    rank(commands, query, recent)
        .into_iter()
        .map(|r| {
            let c = &commands[r.index];
            let mut subtitle = if c.quick {
                "Agent quick action".to_owned()
            } else {
                c.group.title().to_owned()
            };
            if let Some(reason) = &c.reason {
                subtitle = format!("{subtitle} · {reason}");
            } else if !c.vim.is_empty() {
                subtitle = format!("{subtitle} · Vim {}", c.vim.join(", "));
            }
            PickerRow {
                key: c.name.clone(),
                title: c.label.clone(),
                title_positions: r.positions,
                subtitle,
                subtitle_positions: Vec::new(),
                accel: c.accels.join(" "),
                badge: recent.contains(&c.name).then(|| "Recent".to_owned()),
                checked: c.state == Some(true),
                enabled: c.enabled,
            }
        })
        .collect()
}

/// `win.command-palette` (Ctrl+Shift+P).
pub fn show(state: &Rc<WindowState>) {
    let commands = Rc::new(commands(state));
    let recent = Rc::new(recent_for(state));
    Picker::open(
        state,
        PickerSpec {
            kind: KIND,
            placeholder: "Type a command",
            empty_title: "No Matching Commands",
            accessible_label: "Commands",
            filter: Box::new(move |query| command_rows(&commands, query, &recent)),
            choose: Box::new(|state, row| {
                remember(state, &row.key);
                if WidgetExt::activate_action(&state.window, &row.key, None).is_err() {
                    state.toast(&format!("“{}” cannot run here", row.title));
                }
            }),
        },
    );
}

/// True while the command palette of `state`'s window is open.
pub fn is_open(state: &WindowState) -> bool {
    Picker::current(state).is_some_and(|p| p.kind == KIND)
}

/// Types `query` into the open palette or quick open (tests, the demo).
pub fn set_query(state: &WindowState, query: &str) {
    if let Some(picker) = Picker::current(state) {
        picker.entry.set_text(query);
        picker.entry.set_position(-1);
        picker.refresh();
    }
}

/// The keys of the rows shown by the open picker, in order: action names in the palette,
/// absolute paths in quick open.
pub fn visible(state: &WindowState) -> Vec<String> {
    Picker::current(state)
        .map(|p| p.rows.borrow().iter().map(|r| r.key.clone()).collect())
        .unwrap_or_default()
}

/// The rows shown by the open picker (tests).
pub fn visible_rows(state: &WindowState) -> Vec<PickerRow> {
    Picker::current(state)
        .map(|p| p.rows.borrow().clone())
        .unwrap_or_default()
}

/// The key of the selected row of the open picker.
pub fn selected(state: &WindowState) -> Option<String> {
    let picker = Picker::current(state)?;
    let index = picker.selection.selected() as usize;
    picker.rows.borrow().get(index).map(|r| r.key.clone())
}

/// Moves the selection of the open picker by `delta` rows (what Up and Down do).
pub fn move_selection(state: &WindowState, delta: i32) {
    if let Some(picker) = Picker::current(state) {
        picker.move_selection(delta);
    }
}

/// Chooses the selected row of the open picker (what Enter does); false when nothing is
/// selected or the row is disabled.
pub fn activate_selected(state: &WindowState) -> bool {
    Picker::current(state).is_some_and(|p| p.choose(p.selection.selected() as usize))
}

/// Closes the open picker without choosing anything (what Escape does).
pub fn close(state: &WindowState) {
    if let Some(picker) = Picker::current(state) {
        picker.dialog.close();
    }
}

/// The open picker's dialog (tests, snapshots).
pub fn dialog(state: &WindowState) -> Option<adw::Dialog> {
    Picker::current(state).map(|p| p.dialog.clone())
}

/// `--demo palette`: the palette open with "pdf" typed (after one recent command), laid out.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    remember(&state, "win.toggle-pdf");
    show(&state);
    let dialog = dialog(&state).ok_or("the palette did not open")?;
    crate::testmode::wait_until(Duration::from_secs(5), "the palette to be shown", || {
        dialog.is_mapped() && dialog.height() > 0
    })
    .await?;
    // Typed once the entry has the focus (which selects what it holds).
    crate::testmode::wait_for_frames(&state.window, 1).await;
    set_query(&state, "pdf");
    if visible(&state).is_empty() {
        return Err("no command matches \"pdf\"".into());
    }
    crate::testmode::wait_for_frames(&state.window, 1).await;
    Ok(())
}

// ───────────── The picker dialog ─────────────

/// One row of a picker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PickerRow {
    /// What the row stands for (an action name, a path).
    pub key: String,
    pub title: String,
    /// Byte offsets of the title's characters to show in bold (the match).
    pub title_positions: Vec<usize>,
    pub subtitle: String,
    pub subtitle_positions: Vec<usize>,
    /// Accelerators in `adw::ShortcutLabel` syntax ("" for none).
    pub accel: String,
    /// A dim word at the end ("Recent", "Open").
    pub badge: Option<String>,
    /// Shows a check mark (a switch that is on).
    pub checked: bool,
    pub enabled: bool,
}

/// The rows of a picker for a query, best first.
pub(crate) type FilterFn = dyn Fn(&str) -> Vec<PickerRow>;
/// What choosing a row of a picker does.
pub(crate) type ChooseFn = dyn Fn(&Rc<WindowState>, &PickerRow);
/// A window action's function (`show`).
type ShowFn = fn(&Rc<WindowState>);

/// What a picker lists and does.
pub(crate) struct PickerSpec {
    /// Which picker this is (one per window: another kind replaces it).
    pub kind: &'static str,
    pub placeholder: &'static str,
    /// The title of the empty list.
    pub empty_title: &'static str,
    pub accessible_label: &'static str,
    /// The rows for a query, best first.
    pub filter: Box<FilterFn>,
    /// Runs the chosen row, after the dialog closed and the focus went back.
    pub choose: Box<ChooseFn>,
}

/// An open picker dialog (see the module docs).
pub(crate) struct Picker {
    pub kind: &'static str,
    pub dialog: adw::Dialog,
    pub entry: gtk::SearchEntry,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    list: gtk::ListView,
    stack: gtk::Stack,
    rows: RefCell<Vec<PickerRow>>,
    /// The query the rows are for.
    query: RefCell<String>,
    filter: Box<FilterFn>,
    choose: Box<ChooseFn>,
    chosen: RefCell<Option<PickerRow>>,
    closed: Cell<bool>,
    previous_focus: glib::WeakRef<gtk::Widget>,
    state: Weak<WindowState>,
}

thread_local! {
    /// The open picker of each window.
    static OPEN: RefCell<Vec<(glib::WeakRef<adw::ApplicationWindow>, Rc<Picker>)>> =
        const { RefCell::new(Vec::new()) };
}

const CSS: &str = "
dialog.lemma-picker searchentry { min-height: 38px; }
dialog.lemma-picker .lemma-picker-list { background: none; }
dialog.lemma-picker .lemma-picker-list > row { margin: 1px 6px; padding: 6px 10px; }
dialog.lemma-picker .lemma-picker-title { font-weight: normal; }
dialog.lemma-picker .lemma-picker-badge { font-size: 0.85em; }
dialog.lemma-picker shortcut-label { font-size: 0.9em; }
";

impl Picker {
    /// The open picker of `state`'s window.
    pub fn current(state: &WindowState) -> Option<Rc<Picker>> {
        OPEN.with_borrow_mut(|open| {
            open.retain(|(w, p)| w.upgrade().is_some() && !p.closed.get());
            open.iter()
                .find(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
                .map(|(_, p)| p.clone())
        })
    }

    /// Opens a picker (or, when one of the same kind is open, selects its query).
    pub fn open(state: &Rc<WindowState>, spec: PickerSpec) -> Rc<Picker> {
        // Where the focus goes back to: the focus before the first picker of a replacement.
        let previous_focus = glib::WeakRef::new();
        previous_focus.set(gtk::prelude::GtkWindowExt::focus(&state.window).as_ref());
        if let Some(open) = Self::current(state) {
            if open.kind == spec.kind {
                open.entry.grab_focus();
                open.entry.select_region(0, -1);
                return open;
            }
            previous_focus.set(open.previous_focus.upgrade().as_ref());
            open.forget();
            open.dialog.force_close();
        }
        crate::build_ui::install_css_once("lemma-picker", CSS);
        let entry = gtk::SearchEntry::builder()
            .placeholder_text(spec.placeholder)
            .search_delay(0)
            .hexpand(true)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label(spec.placeholder)]);
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::builder()
            .model(&store)
            .autoselect(true)
            .can_unselect(false)
            .build();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&row_widget()));
                item.set_focusable(false);
            }
        });
        factory.connect_bind(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>()
                && let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>()
                && let Some(child) = item.child()
            {
                let row = obj.borrow::<PickerRow>();
                bind_row(&child, &row);
                item.set_activatable(row.enabled);
            }
        });
        let list = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .single_click_activate(true)
            .can_focus(false)
            .css_classes(["navigation-sidebar", "lemma-picker-list"])
            .build();
        list.update_property(&[gtk::accessible::Property::Label(spec.accessible_label)]);
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title(spec.empty_title)
            .css_classes(["compact"])
            .vexpand(true)
            .build();
        let stack = gtk::Stack::new();
        stack.add_named(&scrolled, Some("list"));
        stack.add_named(&empty, Some("empty"));
        let top = gtk::Box::builder()
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        top.append(&entry);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&top);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&stack);
        let dialog = adw::Dialog::builder()
            .content_width(580)
            .content_height(440)
            .title(spec.accessible_label)
            .child(&content)
            .css_classes(["lemma-picker"])
            .build();
        dialog.set_focus(Some(&entry));

        let picker = Rc::new(Picker {
            kind: spec.kind,
            dialog: dialog.clone(),
            entry: entry.clone(),
            store,
            selection,
            list: list.clone(),
            stack,
            rows: RefCell::new(Vec::new()),
            query: RefCell::new(String::new()),
            filter: spec.filter,
            choose: spec.choose,
            chosen: RefCell::new(None),
            closed: Cell::new(false),
            previous_focus,
            state: Rc::downgrade(state),
        });
        OPEN.with_borrow_mut(|open| open.push((state.window.downgrade(), picker.clone())));

        let weak = Rc::downgrade(&picker);
        entry.connect_search_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.refresh();
            }
        });
        let weak = Rc::downgrade(&picker);
        entry.connect_activate(move |_| {
            if let Some(p) = weak.upgrade() {
                p.choose(p.selection.selected() as usize);
            }
        });
        let weak = Rc::downgrade(&picker);
        entry.connect_stop_search(move |_| {
            if let Some(p) = weak.upgrade() {
                p.dialog.close();
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&picker);
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(p) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            // The window's accelerators do not reach a dialog: the palette's and quick open's
            // own keys switch between them here.
            if let Some(state) = p.state.upgrade() {
                let pickers: [(&str, ShowFn); 2] = [
                    ("win.command-palette", show),
                    ("win.quick-open", crate::quick_open::show),
                ];
                for (action, open) in pickers {
                    if accel_matches(&state, action, key, mods) {
                        glib::idle_add_local_once(move || open(&state));
                        return glib::Propagation::Stop;
                    }
                }
            }
            let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
            let page = (p.list.height() / 44).max(1);
            let delta = match key {
                gdk::Key::Up | gdk::Key::KP_Up => -1,
                gdk::Key::Down | gdk::Key::KP_Down => 1,
                gdk::Key::Page_Up | gdk::Key::KP_Page_Up => -page,
                gdk::Key::Page_Down | gdk::Key::KP_Page_Down => page,
                gdk::Key::n | gdk::Key::j if ctrl => 1,
                _ => return glib::Propagation::Proceed,
            };
            p.move_selection(delta);
            glib::Propagation::Stop
        });
        entry.add_controller(keys);
        let weak = Rc::downgrade(&picker);
        list.connect_activate(move |_, position| {
            if let Some(p) = weak.upgrade() {
                p.choose(position as usize);
            }
        });
        let weak = Rc::downgrade(&picker);
        dialog.connect_closed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.closed_now();
            }
        });

        picker.refresh();
        dialog.present(Some(&state.window));
        entry.grab_focus();
        picker
    }

    /// Takes the picker out of the window's registry (it is being replaced, or it closed).
    fn forget(&self) {
        self.closed.set(true);
        self.chosen.replace(None);
        let dialog = self.dialog.clone();
        OPEN.with_borrow_mut(|open| open.retain(|(_, p)| p.dialog != dialog));
    }

    /// Filters again. A new query selects the best match; the same query (new rows arrived)
    /// keeps the selected row when it is still listed.
    pub fn refresh(&self) {
        let query = self.entry.text().to_string();
        let same_query = *self.query.borrow() == query;
        let selected = {
            let rows = self.rows.borrow();
            rows.get(self.selection.selected() as usize)
                .map(|r| r.key.clone())
        };
        let rows = (self.filter)(&query);
        let objects: Vec<glib::BoxedAnyObject> = rows
            .iter()
            .map(|r| glib::BoxedAnyObject::new(r.clone()))
            .collect();
        let keep = selected
            .filter(|_| same_query)
            .and_then(|key| rows.iter().position(|r| r.key == key));
        self.query.replace(query);
        self.rows.replace(rows);
        self.store.splice(0, self.store.n_items(), &objects);
        self.stack
            .set_visible_child_name(if objects.is_empty() { "empty" } else { "list" });
        if !objects.is_empty() {
            let index = keep.unwrap_or(0) as u32;
            self.selection.set_selected(index);
            self.list
                .scroll_to(index, gtk::ListScrollFlags::NONE, None::<gtk::ScrollInfo>);
        }
    }

    /// Moves the selection by `delta` rows, stopping at the ends.
    pub fn move_selection(&self, delta: i32) {
        let n = self.store.n_items() as i32;
        if n == 0 {
            return;
        }
        let current = self.selection.selected() as i32;
        let current = if current >= n { 0 } else { current };
        let index = (current + delta).clamp(0, n - 1) as u32;
        self.selection.set_selected(index);
        self.list
            .scroll_to(index, gtk::ListScrollFlags::NONE, None::<gtk::ScrollInfo>);
    }

    /// Chooses row `index`: closes the dialog, then runs it. A disabled row rings the bell.
    pub fn choose(&self, index: usize) -> bool {
        let Some(row) = self.rows.borrow().get(index).cloned() else {
            self.entry.error_bell();
            return false;
        };
        if !row.enabled {
            self.entry.error_bell();
            return false;
        }
        self.chosen.replace(Some(row));
        self.dialog.close();
        true
    }

    /// The dialog closed: give the focus back, then run the chosen row.
    fn closed_now(self: Rc<Self>) {
        let replaced = self.closed.get();
        let chosen = self.chosen.take();
        self.forget();
        let Some(state) = self.state.upgrade() else {
            return;
        };
        if !replaced
            && let Some(widget) = self.previous_focus.upgrade()
            && widget.root().as_ref() == Some(state.window.upcast_ref::<gtk::Root>())
            && widget.is_mapped()
        {
            widget.grab_focus();
        }
        let Some(row) = chosen.filter(|_| !replaced) else {
            return;
        };
        glib::idle_add_local_once(move || {
            if let Some(state) = self.state.upgrade() {
                (self.choose)(&state, &row);
            }
        });
    }
}

/// True when the key press `key` + `mods` is one of the accelerators of `action` right now.
fn accel_matches(
    state: &WindowState,
    action: &str,
    key: gdk::Key,
    mods: gdk::ModifierType,
) -> bool {
    let relevant = gtk::accelerator_get_default_mod_mask();
    let pressed = (key.to_lower(), mods & relevant);
    state.app.accels_for_action(action).iter().any(|accel| {
        gtk::accelerator_parse(accel.as_str()).is_some_and(|(k, m)| {
            let wanted = (k.to_lower(), m & relevant);
            // Shift may be consumed by the key itself (`?` is Shift+/ on most layouts).
            wanted == pressed
                || (wanted.0 == pressed.0
                    && (wanted.1 | gdk::ModifierType::SHIFT_MASK)
                        == (pressed.1 | gdk::ModifierType::SHIFT_MASK)
                    && key.to_unicode().is_some_and(|c| !c.is_alphabetic()))
        })
    })
}

/// The widgets of one row: titles on the left; the badge, check mark and keycaps on the right.
fn row_widget() -> gtk::Box {
    let title = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .use_markup(true)
        .css_classes(["lemma-picker-title"])
        .build();
    let subtitle = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .use_markup(true)
        .css_classes(["dim-label", "caption"])
        .build();
    let titles = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    titles.append(&title);
    titles.append(&subtitle);
    let badge = gtk::Label::builder()
        .valign(gtk::Align::Center)
        .css_classes(["dim-label", "lemma-picker-badge"])
        .build();
    let check = gtk::Image::builder()
        .icon_name("object-select-symbolic")
        .valign(gtk::Align::Center)
        .build();
    check.update_property(&[gtk::accessible::Property::Label("On")]);
    let shortcut = adw::ShortcutLabel::new("");
    shortcut.set_valign(gtk::Align::Center);
    let row = gtk::Box::builder().spacing(12).build();
    row.append(&titles);
    row.append(&badge);
    row.append(&check);
    row.append(&shortcut);
    row
}

/// Pango markup of `text` with the characters starting at `positions` in bold.
pub(crate) fn highlight_markup(text: &str, positions: &[usize]) -> String {
    if positions.is_empty() {
        return glib::markup_escape_text(text).to_string();
    }
    let mut out = String::new();
    let mut bold = false;
    for (i, c) in text.char_indices() {
        let wanted = positions.binary_search(&i).is_ok();
        if wanted != bold {
            out.push_str(if wanted { "<b>" } else { "</b>" });
            bold = wanted;
        }
        out.push_str(&glib::markup_escape_text(c.encode_utf8(&mut [0; 4])));
    }
    if bold {
        out.push_str("</b>");
    }
    out
}

fn bind_row(child: &gtk::Widget, row: &PickerRow) {
    let Some(titles) = child.first_child() else {
        return;
    };
    let labels = (
        titles.first_child().and_downcast::<gtk::Label>(),
        titles.last_child().and_downcast::<gtk::Label>(),
    );
    if let (Some(title), Some(subtitle)) = labels {
        title.set_markup(&highlight_markup(&row.title, &row.title_positions));
        subtitle.set_markup(&highlight_markup(&row.subtitle, &row.subtitle_positions));
        subtitle.set_visible(!row.subtitle.is_empty());
    }
    let badge = titles.next_sibling();
    if let Some(badge) = badge.clone().and_downcast::<gtk::Label>() {
        badge.set_label(row.badge.as_deref().unwrap_or(""));
        badge.set_visible(row.badge.is_some());
    }
    let check = badge.and_then(|b| b.next_sibling());
    if let Some(check) = &check {
        check.set_visible(row.checked);
    }
    if let Some(shortcut) = check
        .and_then(|c| c.next_sibling())
        .and_downcast::<adw::ShortcutLabel>()
    {
        shortcut.set_accelerator(&row.accel);
        shortcut.set_visible(!row.accel.is_empty());
    }
    child.set_sensitive(row.enabled);
    let mut description = row.title.clone();
    if !row.subtitle.is_empty() {
        description = format!("{description}, {}", row.subtitle);
    }
    child.update_property(&[gtk::accessible::Property::Label(&description)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, label: &str) -> Command {
        Command {
            name: name.into(),
            label: label.into(),
            group: ShortcutGroup::General,
            quick: false,
            accels: Vec::new(),
            vim: Vec::new(),
            enabled: true,
            reason: None,
            state: None,
        }
    }

    #[test]
    fn labels_from_names() {
        assert_eq!(label_from_name("toggle-pdf-sidebar"), "Toggle PDF Sidebar");
        assert_eq!(label_from_name("history_restore"), "History Restore");
        assert_eq!(searchable_name("win.toggle-vim"), "toggle vim");
    }

    #[test]
    fn markup_bolds_matched_characters_and_escapes() {
        assert_eq!(highlight_markup("Save & Go", &[]), "Save &amp; Go");
        assert_eq!(highlight_markup("Save", &[0, 1]), "<b>Sa</b>ve");
        assert_eq!(highlight_markup("a<b", &[1, 2]), "a<b>&lt;b</b>");
        assert_eq!(highlight_markup("Open…", &[4, 5]), "Open<b>…</b>");
        assert_eq!(char_positions("Open…", &[4, 5, 6, 9]), [4]);
    }

    #[test]
    fn empty_query_puts_recent_first() {
        let list = [cmd("win.a", "A"), cmd("win.b", "B"), cmd("win.c", "C")];
        let order: Vec<usize> = rank(&list, "", &["win.c".into(), "win.x".into(), "win.b".into()])
            .iter()
            .map(|r| r.index)
            .collect();
        assert_eq!(order, [2, 1, 0]);
        let order: Vec<usize> = rank(&list, "  ", &[]).iter().map(|r| r.index).collect();
        assert_eq!(order, [0, 1, 2]);
    }

    #[test]
    fn query_words_all_match_and_names_count() {
        let list = [
            cmd("win.toggle-vim", "Vim Mode"),
            cmd("win.pdf-zoom-in", "Zoom In PDF"),
            cmd("win.pdf-zoom-out", "Zoom Out PDF"),
        ];
        let names = |q: &str| -> Vec<&str> {
            rank(&list, q, &[])
                .iter()
                .map(|r| list[r.index].name.as_str())
                .collect()
        };
        assert_eq!(names("pdf in"), ["win.pdf-zoom-in"]);
        assert_eq!(names("toggle vim"), ["win.toggle-vim"]);
        assert_eq!(names("zoom")[..2], ["win.pdf-zoom-in", "win.pdf-zoom-out"]);
        assert!(names("qqq").is_empty());
        let r = &rank(&list, "zoom", &[])[0];
        assert_eq!(r.positions, [0, 1, 2, 3]);
    }
}
