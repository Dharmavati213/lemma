//! The Preferences dialog.
//!
//! `win.preferences` (Ctrl+,) shows an `adw::PreferencesDialog` over the most used settings
//! ([`PAGES`]): the editor font, wrapping, inline equation numbers, Vim, the typing helpers,
//! completion while typing, spell checking and its language (the installed dictionaries); build
//! on save and after agent turns, the PDF preview's dark pages, cursor following and thumbnail
//! strip, hover popovers, the math preview and the hover delay, the outline following the
//! cursor; agent notifications and the local history limits — plus the configuration file's
//! path with an Open button (the system's handler for the file, through `gtk::FileLauncher`;
//! a missing file is created first).
//!
//! # Writing `config.toml`
//!
//! Every change goes through `state.edit_config_file` with [`set_keeping_comments`]: one value
//! is rewritten in place (`lemma_config::set_value`), every comment and blank line stays, and a
//! comment at the end of the changed line stays too; a symbolically linked `config.toml`
//! (dotfiles) is written where it points; the result must be a valid configuration; the new
//! configuration applies to the window at once (`WindowState::set_config`: the shell re-applies
//! the font, wrapping, the PDF strip, … and every module reads its values at use time), so the
//! live reload finds nothing new. Switching Vim also switches it in this window (the file's
//! `editor.vim` alone only decides new windows). Numbers are written [`NUMBER_DEBOUNCE`] after
//! the last change (and when the dialog closes), so holding a spin button down writes once.
//!
//! A write that fails (an unreadable file, `LEMMA_CONFIG=/dev/null`, a value the line editor
//! cannot change such as a multi-line array) is reported in a toast inside the dialog and the
//! control goes back to the configured value. When the configuration is not a file that can
//! be written, the dialog says so and its controls are insensitive.
//!
//! The controls follow the configuration: `connect_config_changed` updates them when the file
//! is edited elsewhere while the dialog is open (without writing anything back).
//!
//! The pages are Editor (font, wrapping, equation numbers, Vim, typing, spelling and the
//! configuration file), Preview (build, PDF, hovers, outline) and Agent & History; the
//! dialog's search finds every row by its title.
//!
//! Hooks: [`show`] is the action; [`run_demo`] is `--demo prefs` (resolves when the dialog is
//! open and drawn, for `m-prefs.png`). Tests use [`PAGES`], [`dialog`], [`set_bool`],
//! [`set_number`], [`set_font`], [`set_language`], [`bool_value`], [`number_value`],
//! [`font_value`], [`language_value`], [`flush`], [`is_writable`],
//! [`set_keeping_comments`] and [`set_open_hook`].

use crate::hooks::HookId;
use crate::settings::ConfigFile;
use crate::window::WindowState;
use adw::prelude::*;
use lemma_config::{Config, ConfigValue};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// How long after the last change of a number it is written.
pub const NUMBER_DEBOUNCE: Duration = Duration::from_millis(400);

// ───────────── What the dialog shows ─────────────

/// What kind of control a setting has.
#[derive(Clone, Copy)]
pub enum Kind {
    /// A switch.
    Bool(fn(&Config) -> bool),
    /// A spin button.
    Number {
        get: fn(&Config) -> i64,
        min: i64,
        max: i64,
        step: i64,
    },
    /// A font button (`editor.font`).
    Font,
    /// The spell-checking language (`spell.language`): the installed dictionaries.
    Language,
}

/// One setting of the dialog: `[section] key`.
#[derive(Clone, Copy)]
pub struct Setting {
    pub section: &'static str,
    pub key: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
    pub kind: Kind,
}

/// A group of settings on a page.
pub struct Group {
    pub title: &'static str,
    pub settings: &'static [Setting],
}

/// A page of the dialog.
pub struct Page {
    pub title: &'static str,
    pub icon: &'static str,
    pub groups: &'static [Group],
}

const fn switch(
    section: &'static str,
    key: &'static str,
    title: &'static str,
    subtitle: &'static str,
    get: fn(&Config) -> bool,
) -> Setting {
    Setting {
        section,
        key,
        title,
        subtitle,
        kind: Kind::Bool(get),
    }
}

/// Every page, group and setting of the dialog, in order.
pub const PAGES: &[Page] = &[
    Page {
        title: "Editor",
        icon: "document-edit-symbolic",
        groups: &[
            Group {
                title: "Text",
                settings: &[
                    Setting {
                        section: "editor",
                        key: "font",
                        title: "Font",
                        subtitle: "",
                        kind: Kind::Font,
                    },
                    switch("editor", "wrap", "Wrap Long Lines", "", |c| c.editor.wrap),
                    switch(
                        "editor",
                        "equation_numbers",
                        "Equation Numbers",
                        "Show (3.2) after each label, from the last build",
                        |c| c.editor.equation_numbers,
                    ),
                ],
            },
            Group {
                title: "Keyboard",
                settings: &[switch(
                    "editor",
                    "vim",
                    "Vim Mode",
                    "Vim keys in the editor; Ctrl+Alt+V switches them for one window",
                    |c| c.editor.vim,
                )],
            },
            Group {
                title: "Typing",
                settings: &[
                    switch(
                        "typing",
                        "autopair",
                        "Close Brackets",
                        "Type braces, brackets and parentheses in pairs",
                        |c| c.typing.autopair,
                    ),
                    switch(
                        "typing",
                        "smart_dollar",
                        "Smart Dollar Signs",
                        "$ pairs in text and steps over a closing $",
                        |c| c.typing.smart_dollar,
                    ),
                    switch(
                        "typing",
                        "auto_end",
                        "Close Environments",
                        "Add \\end{…} when a \\begin{…} is complete",
                        |c| c.typing.auto_end,
                    ),
                    switch(
                        "typing",
                        "item_on_enter",
                        "Continue Lists",
                        "Enter in a list starts the next \\item",
                        |c| c.typing.item_on_enter,
                    ),
                    switch(
                        "typing",
                        "autosnippets",
                        "Automatic Snippets",
                        "Expand snippets such as ff → \\frac{}{} while typing",
                        |c| c.typing.autosnippets,
                    ),
                    switch(
                        "completion",
                        "auto",
                        "Complete Words While Typing",
                        "Commands, references and citations always complete",
                        |c| c.completion.auto,
                    ),
                ],
            },
            Group {
                title: "Spelling",
                settings: &[
                    switch("spell", "enabled", "Check Spelling", "", |c| {
                        c.spell.enabled
                    }),
                    Setting {
                        section: "spell",
                        key: "language",
                        title: "Language",
                        subtitle: "",
                        kind: Kind::Language,
                    },
                ],
            },
        ],
    },
    Page {
        title: "Preview",
        icon: "x-office-document-symbolic",
        groups: &[
            Group {
                title: "Build",
                settings: &[
                    switch("build", "on_save", "Build on Save", "", |c| c.build.on_save),
                    switch(
                        "build",
                        "on_agent_turn_end",
                        "Build After Agent Turns",
                        "When a turn of the agent changed files",
                        |c| c.build.on_agent_turn_end,
                    ),
                ],
            },
            Group {
                title: "PDF",
                settings: &[
                    switch(
                        "pdf",
                        "dark_invert",
                        "Dark Pages",
                        "Invert the pages while the style is dark",
                        |c| c.pdf.dark_invert,
                    ),
                    switch(
                        "pdf",
                        "follow_cursor",
                        "Follow the Cursor",
                        "Show the cursor's line in the PDF while typing",
                        |c| c.pdf.follow_cursor,
                    ),
                    switch(
                        "pdf",
                        "thumbnails",
                        "Thumbnails and Bookmarks",
                        "The strip at the left of the preview",
                        |c| c.pdf.thumbnails,
                    ),
                ],
            },
            Group {
                title: "Hovers",
                settings: &[
                    switch(
                        "editor",
                        "hover",
                        "Hover Popovers",
                        "References, citations and macros under the pointer",
                        |c| c.editor.hover,
                    ),
                    switch(
                        "preview",
                        "math_hover",
                        "Math Preview",
                        "Render the formula under the pointer",
                        |c| c.preview.math_hover,
                    ),
                    switch(
                        "preview",
                        "live",
                        "Live Math Preview",
                        "While typing inside math, show the rendered formula below it",
                        |c| c.preview.live,
                    ),
                    Setting {
                        section: "preview",
                        key: "delay_ms",
                        title: "Hover Delay",
                        subtitle: "Milliseconds before a popover appears",
                        kind: Kind::Number {
                            get: |c| i64::from(c.preview.delay_ms),
                            min: 0,
                            max: 2000,
                            step: 50,
                        },
                    },
                ],
            },
            Group {
                title: "Outline",
                settings: &[switch(
                    "outline",
                    "follow_cursor",
                    "Follow the Cursor",
                    "Select the section the cursor is in",
                    |c| c.outline.follow_cursor,
                )],
            },
        ],
    },
    Page {
        title: "Agent & History",
        icon: "document-open-recent-symbolic",
        groups: &[
            Group {
                title: "Agent",
                settings: &[switch(
                    "agent",
                    "notify_on_turn_end",
                    "Notify When a Turn Ends",
                    "A desktop notification while Lemma is in the background",
                    |c| c.agent.notify_on_turn_end,
                )],
            },
            Group {
                title: "Local History",
                settings: &[
                    switch(
                        "history",
                        "enabled",
                        "Keep Local History",
                        "Snapshots of your files on save and before every agent turn",
                        |c| c.history.enabled,
                    ),
                    Setting {
                        section: "history",
                        key: "max_mib",
                        title: "Size Limit",
                        subtitle: "MiB per project; the oldest snapshots go first",
                        kind: Kind::Number {
                            get: |c| i64::from(c.history.max_mib),
                            min: 10,
                            max: 100_000,
                            step: 10,
                        },
                    },
                    Setting {
                        section: "history",
                        key: "max_days",
                        title: "Keep Snapshots For",
                        subtitle: "Days",
                        kind: Kind::Number {
                            get: |c| i64::from(c.history.max_days),
                            min: 1,
                            max: 3650,
                            step: 1,
                        },
                    },
                    Setting {
                        section: "history",
                        key: "min_interval_s",
                        title: "Snapshot Interval",
                        subtitle: "Seconds between two snapshots of a file on save",
                        kind: Kind::Number {
                            get: |c| i64::from(c.history.min_interval_s),
                            min: 0,
                            max: 86_400,
                            step: 10,
                        },
                    },
                ],
            },
        ],
    },
];

/// Every setting of [`PAGES`], in order.
pub fn settings() -> impl Iterator<Item = &'static Setting> {
    PAGES
        .iter()
        .flat_map(|p| p.groups.iter())
        .flat_map(|g| g.settings.iter())
}

// ───────────── Writing the file ─────────────

/// The comment at the end of a TOML line (with the blanks before it), outside quotes.
fn trailing_comment(line: &str) -> Option<&str> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '#' => {
                let start = line[..i].trim_end().len();
                return Some(&line[start..]);
            }
            None => {}
        }
    }
    None
}

/// `lemma_config::set_value`, keeping what it would drop of the line it rewrites: the line's
/// indentation and the comment at its end. Every other line stays as written anyway.
pub fn set_keeping_comments(
    text: &str,
    section: &str,
    key: &str,
    value: ConfigValue<'_>,
) -> Result<String, String> {
    let new = lemma_config::set_value(text, section, key, value)?;
    let old_lines: Vec<&str> = text.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len() != new_lines.len() {
        // A line was added, none replaced.
        return Ok(new);
    }
    let changed: Vec<usize> = (0..old_lines.len())
        .filter(|&i| old_lines[i] != new_lines[i])
        .collect();
    let [i] = changed[..] else {
        return Ok(new);
    };
    let old = old_lines[i];
    let indent = &old[..old.len() - old.trim_start().len()];
    let comment = trailing_comment(old).unwrap_or("");
    if indent.is_empty() && comment.is_empty() {
        return Ok(new);
    }
    let mut lines: Vec<String> = new_lines.iter().map(|l| (*l).to_owned()).collect();
    lines[i] = format!("{indent}{}{comment}", new_lines[i]);
    let mut kept = lines.join("\n");
    kept.push('\n');
    // Only when it means the same.
    match (lemma_config::parse(&kept), lemma_config::parse(&new)) {
        (Ok(a), Ok(b)) if a == b => Ok(kept),
        _ => Ok(new),
    }
}

/// True when the configuration file can be written: there is one, and it is a regular file (or
/// missing, or a link to one).
pub fn is_writable(state: &WindowState) -> bool {
    state
        .config_file()
        .is_some_and(|path| match std::fs::metadata(&path) {
            Ok(m) => m.is_file(),
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        })
}

// ───────────── The dialog ─────────────

/// The control of one setting.
#[derive(Clone)]
enum Control {
    Switch(adw::SwitchRow),
    Spin(adw::SpinRow),
    Font(gtk::FontDialogButton),
    /// The combo row and the language codes of its items.
    Language(adw::ComboRow, Rc<RefCell<Vec<String>>>),
}

/// An open Preferences dialog.
struct Prefs {
    dialog: adw::PreferencesDialog,
    state: Weak<WindowState>,
    controls: RefCell<Vec<(&'static Setting, Control)>>,
    /// True while the controls are being set from the configuration (nothing is written).
    syncing: Cell<bool>,
    /// Numbers waiting to be written, by `section.key`.
    pending: RefCell<HashMap<String, (glib::SourceId, i64)>>,
    hook: Cell<Option<HookId>>,
}

/// The open Preferences dialog of each window.
type OpenDialogs = Vec<(glib::WeakRef<adw::ApplicationWindow>, Rc<Prefs>)>;
/// What the configuration file's Open button calls instead of the system's handler (tests).
pub type OpenHookFn = dyn Fn(&Path);
type OpenHook = Rc<OpenHookFn>;

thread_local! {
    static OPEN: RefCell<OpenDialogs> = const { RefCell::new(Vec::new()) };
    /// Tests: what the Open button opens.
    static OPEN_HOOK: RefCell<Option<OpenHook>> = const { RefCell::new(None) };
}

fn current(state: &WindowState) -> Option<Rc<Prefs>> {
    OPEN.with_borrow_mut(|open| {
        open.retain(|(w, _)| w.upgrade().is_some());
        open.iter()
            .find(|(w, _)| w.upgrade().as_ref() == Some(&state.window))
            .map(|(_, p)| p.clone())
    })
}

/// Makes the configuration file's Open button call `hook` instead of launching the system's
/// handler (tests); `None` restores the default.
pub fn set_open_hook(hook: Option<Box<OpenHookFn>>) {
    OPEN_HOOK.with_borrow_mut(|h| *h = hook.map(Rc::from));
}

/// The open Preferences dialog of `state`'s window.
pub fn dialog(state: &WindowState) -> Option<adw::PreferencesDialog> {
    current(state).map(|p| p.dialog.clone())
}

/// `win.preferences` (Ctrl+,).
pub fn show(state: &Rc<WindowState>) {
    if current(state).is_some() {
        return;
    }
    let prefs = Rc::new(Prefs {
        dialog: adw::PreferencesDialog::builder()
            .title("Preferences")
            .search_enabled(true)
            .build(),
        state: Rc::downgrade(state),
        controls: RefCell::new(Vec::new()),
        syncing: Cell::new(false),
        pending: RefCell::new(HashMap::new()),
        hook: Cell::new(None),
    });
    prefs.build(state);
    OPEN.with_borrow_mut(|open| open.push((state.window.downgrade(), prefs.clone())));
    let weak = Rc::downgrade(&prefs);
    prefs
        .hook
        .set(Some(state.connect_config_changed(move |_, file| {
            if file == ConfigFile::Config
                && let Some(prefs) = weak.upgrade()
            {
                prefs.sync();
            }
        })));
    let weak = Rc::downgrade(&prefs);
    prefs.dialog.connect_closed(move |_| {
        if let Some(prefs) = weak.upgrade() {
            prefs.closed();
        }
    });
    prefs.dialog.present(Some(&state.window));
}

impl Prefs {
    fn build(self: &Rc<Self>, state: &Rc<WindowState>) {
        let writable = is_writable(state);
        for (n, page) in PAGES.iter().enumerate() {
            let p = adw::PreferencesPage::builder()
                .title(page.title)
                .icon_name(page.icon)
                .build();
            for group in page.groups {
                let g = adw::PreferencesGroup::builder().title(group.title).build();
                for setting in group.settings {
                    let (row, control) = self.control(state, setting);
                    g.add(&row);
                    self.controls.borrow_mut().push((setting, control));
                }
                g.set_sensitive(writable);
                p.add(&g);
            }
            if n == 0 {
                p.add(&self.file_group(state, writable));
            }
            self.dialog.add(&p);
        }
        self.sync();
    }

    /// The row and control of `setting`, connected to write it.
    fn control(
        self: &Rc<Self>,
        state: &Rc<WindowState>,
        setting: &'static Setting,
    ) -> (gtk::Widget, Control) {
        match setting.kind {
            Kind::Bool(_) => {
                let row = adw::SwitchRow::builder()
                    .title(setting.title)
                    .use_underline(false)
                    .build();
                if !setting.subtitle.is_empty() {
                    row.set_subtitle(setting.subtitle);
                }
                let weak = Rc::downgrade(self);
                row.connect_active_notify(move |row| {
                    if let Some(prefs) = weak.upgrade() {
                        prefs.write(setting, ConfigValue::Bool(row.is_active()));
                    }
                });
                (row.clone().upcast(), Control::Switch(row))
            }
            Kind::Number { min, max, step, .. } => {
                let row = adw::SpinRow::with_range(min as f64, max as f64, step as f64);
                row.set_title(setting.title);
                if !setting.subtitle.is_empty() {
                    row.set_subtitle(setting.subtitle);
                }
                row.set_digits(0);
                row.set_numeric(true);
                let weak = Rc::downgrade(self);
                row.connect_value_notify(move |row| {
                    if let Some(prefs) = weak.upgrade() {
                        prefs.write_later(setting, row.value().round() as i64);
                    }
                });
                (row.clone().upcast(), Control::Spin(row))
            }
            Kind::Font => {
                let dialog = gtk::FontDialog::builder()
                    .title("Editor Font")
                    .modal(true)
                    .build();
                let filter = gtk::CustomFilter::new(|item| {
                    let family = item
                        .downcast_ref::<pango::FontFamily>()
                        .cloned()
                        .or_else(|| {
                            item.downcast_ref::<pango::FontFace>()
                                .map(pango::FontFace::family)
                        });
                    family.is_none_or(|f| f.is_monospace())
                });
                dialog.set_filter(Some(&filter));
                let button = gtk::FontDialogButton::builder()
                    .dialog(&dialog)
                    .level(gtk::FontLevel::Font)
                    .use_font(true)
                    .use_size(false)
                    .valign(gtk::Align::Center)
                    .build();
                let row = adw::ActionRow::builder()
                    .title(setting.title)
                    .subtitle("Monospaced fonts")
                    .activatable_widget(&button)
                    .build();
                row.add_suffix(&button);
                let weak = Rc::downgrade(self);
                button.connect_font_desc_notify(move |button| {
                    if let Some(prefs) = weak.upgrade()
                        && let Some(desc) = button.font_desc()
                    {
                        prefs.write(setting, ConfigValue::Str(&desc.to_string()));
                    }
                });
                (row.upcast(), Control::Font(button))
            }
            Kind::Language => {
                let codes = Rc::new(RefCell::new(Vec::new()));
                // Searchable by the language's name.
                let expression = gtk::PropertyExpression::new(
                    gtk::StringObject::static_type(),
                    None::<&gtk::Expression>,
                    "string",
                );
                // The language's name under the title: long names are not cut off.
                let row = adw::ComboRow::builder()
                    .title(setting.title)
                    .enable_search(true)
                    .expression(&expression)
                    .use_subtitle(true)
                    .build();
                fill_languages(&row, &codes, &state.config.spell.language);
                let weak = Rc::downgrade(self);
                let list = codes.clone();
                row.connect_selected_notify(move |row| {
                    let code = list.borrow().get(row.selected() as usize).cloned();
                    if let (Some(prefs), Some(code)) = (weak.upgrade(), code) {
                        prefs.write(setting, ConfigValue::Str(&code));
                    }
                });
                (row.clone().upcast(), Control::Language(row, codes))
            }
        }
    }

    /// The group with the configuration file's path and Open button.
    fn file_group(&self, state: &Rc<WindowState>, writable: bool) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Configuration File")
            .build();
        let path = state.config_file();
        let shown = path.as_deref().map_or_else(
            || "No configuration file (no home directory)".to_owned(),
            |p| tilde(p, state.paths.as_ref().map(|p| p.home.as_path())),
        );
        if !writable {
            group.set_description(Some(
                "Lemma reads its settings from a place it cannot write, so they cannot be \
                 changed here",
            ));
        }
        let row = adw::ActionRow::builder()
            .title(
                path.as_deref()
                    .and_then(Path::file_name)
                    .map_or_else(|| "config.toml".into(), |n| n.to_string_lossy()),
            )
            .subtitle(&shown)
            .subtitle_selectable(true)
            .build();
        let button = gtk::Button::builder()
            .label("_Open")
            .use_underline(true)
            .valign(gtk::Align::Center)
            .sensitive(writable && path.is_some())
            .build();
        button.update_property(&[gtk::accessible::Property::Label(
            "Open the configuration file",
        )]);
        let weak = self.state.clone();
        button.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                open_config_file(&state);
            }
        });
        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        group.add(&row);
        group
    }

    /// Sets every control to the configuration's value, without writing.
    fn sync(&self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let config = state.config.get();
        self.syncing.set(true);
        for (setting, control) in self.controls.borrow().iter() {
            // A number still waiting to be written shows what the user chose.
            if self
                .pending
                .borrow()
                .contains_key(&format!("{}.{}", setting.section, setting.key))
            {
                continue;
            }
            match (setting.kind, control) {
                (Kind::Bool(get), Control::Switch(row)) => {
                    if row.is_active() != get(&config) {
                        row.set_active(get(&config));
                    }
                }
                (Kind::Number { get, .. }, Control::Spin(row)) => {
                    let value = get(&config) as f64;
                    if row.value() != value {
                        row.set_value(value);
                    }
                }
                (Kind::Font, Control::Font(button)) => {
                    let desc = pango::FontDescription::from_string(&config.editor.font);
                    if button
                        .font_desc()
                        .is_none_or(|d| d.to_string() != desc.to_string())
                    {
                        button.set_font_desc(&desc);
                    }
                }
                (Kind::Language, Control::Language(row, codes)) => {
                    let wanted = config.spell.language.clone();
                    let index = codes.borrow().iter().position(|c| *c == wanted);
                    match index {
                        Some(i) if row.selected() as usize == i => {}
                        Some(i) => row.set_selected(i as u32),
                        None => {
                            fill_languages(row, codes, &wanted);
                            if let Some(i) = codes.borrow().iter().position(|c| *c == wanted) {
                                row.set_selected(i as u32);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.syncing.set(false);
    }

    /// Writes `value` to `[setting.section] setting.key` (see the module docs).
    fn write(&self, setting: &Setting, value: ConfigValue<'_>) {
        if self.syncing.get() {
            return;
        }
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let result = state.edit_config_file(|text| {
            set_keeping_comments(text, setting.section, setting.key, value)
        });
        match result {
            Ok(()) => {
                if (setting.section, setting.key) == ("editor", "vim")
                    && let ConfigValue::Bool(on) = value
                    && state.vim_enabled() != on
                {
                    state.set_vim_enabled(on);
                }
            }
            Err(e) => {
                tracing::warn!("preferences: {}.{}: {e}", setting.section, setting.key);
                let message = format!("Could not change “{}”: {e}", setting.title);
                self.dialog.add_toast(
                    adw::Toast::builder()
                        .title(&message)
                        .use_markup(false)
                        .timeout(8)
                        .build(),
                );
                self.sync();
            }
        }
    }

    /// Writes a number [`NUMBER_DEBOUNCE`] after its last change.
    fn write_later(self: &Rc<Self>, setting: &'static Setting, value: i64) {
        if self.syncing.get() {
            return;
        }
        let id = format!("{}.{}", setting.section, setting.key);
        let waiting = self.pending.borrow_mut().remove(&id);
        if let Some((timer, _)) = waiting {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        let key = id.clone();
        let timer = glib::timeout_add_local_once(NUMBER_DEBOUNCE, move || {
            let Some(prefs) = weak.upgrade() else {
                return;
            };
            // Not borrowed while writing: the write updates the controls.
            let waiting = prefs.pending.borrow_mut().remove(&key);
            if let Some((_, value)) = waiting {
                prefs.write(setting, ConfigValue::Int(value));
            }
        });
        self.pending.borrow_mut().insert(id, (timer, value));
    }

    /// Writes the numbers that wait.
    fn flush(&self) {
        let pending: Vec<(String, i64)> = self
            .pending
            .borrow_mut()
            .drain()
            .map(|(id, (timer, value))| {
                timer.remove();
                (id, value)
            })
            .collect();
        for (id, value) in pending {
            if let Some(setting) = settings().find(|s| format!("{}.{}", s.section, s.key) == id) {
                self.write(setting, ConfigValue::Int(value));
            }
        }
    }

    fn closed(&self) {
        self.flush();
        if let (Some(state), Some(hook)) = (self.state.upgrade(), self.hook.take()) {
            state.disconnect(hook);
        }
        let dialog = self.dialog.clone();
        OPEN.with_borrow_mut(|open| open.retain(|(_, p)| p.dialog != dialog));
    }

    fn find(&self, section: &str, key: &str) -> Option<Control> {
        self.controls
            .borrow()
            .iter()
            .find(|(s, _)| s.section == section && s.key == key)
            .map(|(_, c)| c.clone())
    }
}

/// `path` with the home directory written as `~`.
fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

/// Fills the language row with the installed dictionaries (by name), plus `wanted` when it is
/// not installed; selects `wanted`.
fn fill_languages(row: &adw::ComboRow, codes: &Rc<RefCell<Vec<String>>>, wanted: &str) {
    let provider = libspelling::Provider::default();
    let model = provider.list_languages();
    let mut languages: Vec<(String, String)> = (0..model.n_items())
        .filter_map(|i| model.item(i).and_downcast::<libspelling::Language>())
        .filter_map(|l| {
            let code = l.code()?.to_string();
            let name = l.name().map_or_else(|| code.clone(), |n| n.to_string());
            Some((name, code))
        })
        .collect();
    languages.sort();
    languages.dedup_by(|a, b| a.1 == b.1);
    if !wanted.is_empty() && !languages.iter().any(|(_, c)| c == wanted) {
        languages.push((format!("{wanted} (not installed)"), wanted.to_owned()));
    }
    let names: Vec<String> = languages
        .iter()
        .map(|(name, code)| {
            if name.contains(code.as_str()) {
                name.clone()
            } else {
                format!("{name} ({code})")
            }
        })
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    codes.replace(languages.into_iter().map(|(_, c)| c).collect());
    row.set_model(Some(&gtk::StringList::new(&names)));
    if let Some(i) = codes.borrow().iter().position(|c| c == wanted) {
        row.set_selected(i as u32);
    }
}

/// The Open button: the configuration file in the system's handler (created first when it does
/// not exist).
fn open_config_file(state: &Rc<WindowState>) {
    let Some(path) = state.config_file() else {
        return;
    };
    if !path.exists() {
        let created = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::write(
                    &path,
                    "# Lemma's settings. Every key is optional; Preferences (Ctrl+,) writes here.\n",
                )
            });
        if let Err(e) = created {
            state.toast(&format!("Could not create {}: {e}", path.display()));
            return;
        }
    }
    if let Some(hook) = OPEN_HOOK.with_borrow(Clone::clone) {
        hook(&path);
        return;
    }
    if state.scripted {
        return;
    }
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
    let state = state.clone();
    glib::spawn_future_local(async move {
        if let Err(e) = launcher.launch_future(Some(&state.window)).await
            && !e.matches(gtk::DialogError::Dismissed)
        {
            state.toast(&format!("Could not open {}: {e}", path.display()));
        }
    });
}

// ───────────── Test API ─────────────

/// Sets the switch of `[section] key` as the user would (it writes the file); false when the
/// dialog is not open or has no such switch.
pub fn set_bool(state: &WindowState, section: &str, key: &str, on: bool) -> bool {
    match current(state).and_then(|p| p.find(section, key)) {
        Some(Control::Switch(row)) => {
            row.set_active(on);
            true
        }
        _ => false,
    }
}

/// Sets the spin button of `[section] key` (written after [`NUMBER_DEBOUNCE`], or [`flush`]).
pub fn set_number(state: &WindowState, section: &str, key: &str, value: i64) -> bool {
    match current(state).and_then(|p| p.find(section, key)) {
        Some(Control::Spin(row)) => {
            row.set_value(value as f64);
            true
        }
        _ => false,
    }
}

/// Chooses the editor font `desc` ("Monospace 13") in the font button.
pub fn set_font(state: &WindowState, desc: &str) -> bool {
    match current(state).and_then(|p| p.find("editor", "font")) {
        Some(Control::Font(button)) => {
            button.set_font_desc(&pango::FontDescription::from_string(desc));
            true
        }
        _ => false,
    }
}

/// Chooses the spelling language `code` in the combo row; false when it is not listed.
pub fn set_language(state: &WindowState, code: &str) -> bool {
    match current(state).and_then(|p| p.find("spell", "language")) {
        Some(Control::Language(row, codes)) => {
            let index = codes.borrow().iter().position(|c| c == code);
            index.is_some_and(|i| {
                row.set_selected(i as u32);
                true
            })
        }
        _ => false,
    }
}

/// The state of the switch of `[section] key`.
pub fn bool_value(state: &WindowState, section: &str, key: &str) -> Option<bool> {
    match current(state)?.find(section, key)? {
        Control::Switch(row) => Some(row.is_active()),
        _ => None,
    }
}

/// The value of the spin button of `[section] key`.
pub fn number_value(state: &WindowState, section: &str, key: &str) -> Option<i64> {
    match current(state)?.find(section, key)? {
        Control::Spin(row) => Some(row.value().round() as i64),
        _ => None,
    }
}

/// The font the font button shows.
pub fn font_value(state: &WindowState) -> Option<String> {
    match current(state)?.find("editor", "font")? {
        Control::Font(button) => button.font_desc().map(|d| d.to_string()),
        _ => None,
    }
}

/// The language code the language row shows, and every code it lists.
pub fn language_value(state: &WindowState) -> Option<(String, Vec<String>)> {
    match current(state)?.find("spell", "language")? {
        Control::Language(row, codes) => {
            let codes = codes.borrow().clone();
            Some((codes.get(row.selected() as usize)?.clone(), codes))
        }
        _ => None,
    }
}

/// Writes the numbers that wait for [`NUMBER_DEBOUNCE`] now.
pub fn flush(state: &WindowState) {
    if let Some(prefs) = current(state) {
        prefs.flush();
    }
}

/// Activates the configuration file's Open button (tests; see [`set_open_hook`]).
pub fn open_file(state: &Rc<WindowState>) {
    open_config_file(state);
}

/// The configuration file Preferences writes (`state.config_file()`), for messages.
pub fn config_path(state: &WindowState) -> Option<PathBuf> {
    state.config_file()
}

/// `--demo prefs`: the dialog open on its first page, drawn.
pub async fn run_demo(state: Rc<WindowState>) -> Result<(), String> {
    show(&state);
    let dialog = dialog(&state).ok_or("Preferences did not open")?;
    crate::testmode::wait_until(Duration::from_secs(5), "Preferences to be shown", || {
        dialog.is_mapped() && dialog.height() > 0
    })
    .await?;
    crate::testmode::wait_for_frames(&state.window, 1).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_comments_outside_quotes() {
        assert_eq!(trailing_comment("vim = false # mine"), Some(" # mine"));
        assert_eq!(trailing_comment("a = \"x # y\""), None);
        assert_eq!(trailing_comment("a = 'x # y'  # c"), Some("  # c"));
        assert_eq!(trailing_comment("a = \"q\\\" # \" #c"), Some(" #c"));
        assert_eq!(trailing_comment("a = [\"#\", 'b']"), None);
        assert_eq!(trailing_comment("a = 1"), None);
    }

    #[test]
    fn set_keeps_the_comment_and_indentation_of_the_line() {
        let src = "# top\n[editor]\n  vim = false   # no vim\nfont = \"Mono 11\" # big\n";
        let out = set_keeping_comments(src, "editor", "vim", ConfigValue::Bool(true)).unwrap();
        assert_eq!(
            out,
            "# top\n[editor]\n  vim = true   # no vim\nfont = \"Mono 11\" # big\n"
        );
        let out =
            set_keeping_comments(&out, "editor", "font", ConfigValue::Str("A # B 12")).unwrap();
        assert!(out.ends_with("font = \"A # B 12\" # big\n"), "{out}");
        assert_eq!(lemma_config::parse(&out).unwrap().editor.font, "A # B 12");
        // A new key: nothing replaced, the rest as written.
        let out = set_keeping_comments(&out, "pdf", "thumbnails", ConfigValue::Bool(true)).unwrap();
        assert!(
            out.starts_with("# top\n[editor]\n  vim = true   # no vim\n"),
            "{out}"
        );
        assert!(out.ends_with("\n[pdf]\nthumbnails = true\n"), "{out}");
        // Refusals pass through.
        assert!(
            set_keeping_comments("[editor\n", "editor", "vim", ConfigValue::Bool(true)).is_err()
        );
    }

    #[test]
    fn every_setting_is_a_key_of_the_configuration() {
        let defaults = Config::default();
        let mut seen = std::collections::HashSet::new();
        for s in settings() {
            assert!(
                seen.insert((s.section, s.key)),
                "{}.{} twice",
                s.section,
                s.key
            );
            let value = match s.kind {
                Kind::Bool(get) => ConfigValue::Bool(!get(&defaults)),
                Kind::Number { get, min, max, .. } => {
                    let v = get(&defaults);
                    assert!(
                        (min..=max).contains(&v),
                        "{}.{} default {v}",
                        s.section,
                        s.key
                    );
                    ConfigValue::Int(if v == max { min } else { v + 1 })
                }
                Kind::Font => ConfigValue::Str("Mono 12"),
                Kind::Language => ConfigValue::Str("de_DE"),
            };
            let text = set_keeping_comments("", s.section, s.key, value).unwrap();
            let config = lemma_config::parse(&text).unwrap();
            assert_ne!(
                config, defaults,
                "{}.{} changes the configuration",
                s.section, s.key
            );
            if let Kind::Bool(get) = s.kind {
                assert_eq!(get(&config), !get(&defaults), "{}.{}", s.section, s.key);
            }
            if let Kind::Number { get, .. } = s.kind {
                assert_ne!(get(&config), get(&defaults), "{}.{}", s.section, s.key);
            }
        }
        assert_eq!(PAGES.len(), 3);
        assert!(seen.len() >= 22, "{}", seen.len());
    }
}
