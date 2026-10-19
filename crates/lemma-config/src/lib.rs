//! Schema and defaults of `~/.config/lemma/config.toml`.
//!
//! The crate never touches the file system: the app reads the file and passes the text to
//! [`parse`], and [`Paths`] only computes the XDG locations from environment variables
//! ([`Paths::from_vars`] takes the lookup as a function, for tests).
//!
//! Every section and every key is optional, so an empty or partial file is valid: whatever is
//! missing takes the value of [`Config::default`]. Unknown sections and keys are errors, so a typo
//! is reported instead of being silently ignored.
//!
//! ```toml
//! [editor]
//! vim = true
//! font = "JetBrains Mono 12"
//!
//! [keys]
//! "win.build" = "<Control>b"
//! ```
//!
//! Besides `config.toml`, the user's `prompts.toml` (agent quick actions, [`parse_prompts`]) and
//! `snippets.toml` (parsed by `lemma-snippets`) live in the same directory. [`set_string`] and
//! [`set_value`] edit one value of a config file's text without disturbing the rest of it
//! (comments, order); [`remove_key`] deletes one, so it takes its default again.

mod edit;
mod paths;
mod prompts;

pub use edit::{ConfigValue, remove_key, set_string, set_value};
pub use paths::{Paths, expand_tilde};
pub use prompts::{
    PromptContext, PromptTemplate, PromptVars, default_prompts, fill_prompt, find_prompt,
    merge_prompts, parse_prompts,
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The whole configuration file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub editor: EditorCfg,
    pub build: BuildCfg,
    pub pdf: PdfCfg,
    pub agent: AgentCfg,
    pub spell: SpellCfg,
    pub keys: KeysCfg,
    pub typing: TypingCfg,
    pub completion: CompletionCfg,
    pub vim: VimCfg,
    pub history: HistoryCfg,
    // M3 (WP3.0)
    pub outline: OutlineCfg,
    pub preview: PreviewCfg,
    pub lint: LintCfg,
    pub status: StatusCfg,
    pub git: GitCfg,
    pub session: SessionCfg,
}

/// `[editor]`: the text view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorCfg {
    /// Vim emulation (GtkSourceView's VimIMContext) on start-up.
    pub vim: bool,
    /// Pango font description, e.g. `"Monospace 11"`.
    pub font: String,
    /// GtkSourceView style scheme id used with a light system style.
    pub scheme_light: String,
    /// GtkSourceView style scheme id used with a dark system style.
    pub scheme_dark: String,
    /// Extra math environments on top of the VimTeX rules (scanner, M2).
    pub math_envs: Vec<String>,
    /// Soft-wrap long lines.
    pub wrap: bool,
    /// Opacity, in percent, of the background tint the scanner highlighter gives math (its
    /// colour is the style scheme's `def:special-constant`); 0 turns the tint off.
    pub math_tint: u8,
    /// Show a dim `(3.2)` at the end of each line holding a `\label`, from the `.aux` file
    /// (M3).
    pub equation_numbers: bool,
    /// Show hover popovers when the pointer rests on a reference, citation, macro or formula
    /// (M3). `K` in Vim normal mode and Alt+K show them at the cursor either way.
    pub hover: bool,
    /// The GTK renderer, chosen at start-up (M5); the `GSK_RENDERER` environment variable wins
    /// when it is set.
    pub renderer: Renderer,
    /// Save edited documents by themselves (M5, WP5.5): never, when the window loses the focus,
    /// or two seconds after the last change.
    pub autosave: Autosave,
}

/// `[editor] autosave` (WP5.5): when Lemma saves edited documents without being asked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autosave {
    /// Only Ctrl+S, `:w` and builds save.
    #[default]
    Off,
    /// When the window loses the keyboard focus.
    Focus,
    /// Two seconds after the last change.
    Idle,
}

/// `[editor] renderer`: how GTK draws the windows (read at start-up only).
///
/// `auto` is cairo, from measurements of the release build on a Wayland desktop: against GTK's
/// default Vulkan renderer it needs about 27 MB less (no GPU driver stack) and shows the first
/// frame about 140 ms sooner, types as fast (≤ 3 ms of work per keystroke at p99), and still
/// scrolls the PDF at 60 fps, though with less headroom (13–18 ms a frame against Vulkan's 2 ms).
/// `vulkan` suits large or HiDPI screens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Renderer {
    /// Lemma's choice: cairo.
    #[default]
    Auto,
    /// GTK's software renderer: the least memory and the quickest start.
    Cairo,
    /// GTK's Vulkan renderer.
    Vulkan,
    /// GTK's OpenGL renderer (`ngl`).
    Gl,
}

impl Renderer {
    /// The value for `GSK_RENDERER`.
    pub fn gsk_name(self) -> &'static str {
        match self {
            Self::Auto | Self::Cairo => "cairo",
            Self::Vulkan => "vulkan",
            Self::Gl => "ngl",
        }
    }
}

impl Default for EditorCfg {
    fn default() -> Self {
        Self {
            vim: false,
            font: "Monospace 11".into(),
            scheme_light: "Adwaita".into(),
            scheme_dark: "Adwaita-dark".into(),
            math_envs: Vec::new(),
            wrap: true,
            math_tint: 6,
            equation_numbers: true,
            hover: true,
            renderer: Renderer::Auto,
            autosave: Autosave::Off,
        }
    }
}

/// `[build]`: latexmk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildCfg {
    /// Build after every save (Ctrl+S, `:w`).
    pub on_save: bool,
    /// Build when an agent turn that changed files ends.
    pub on_agent_turn_end: bool,
    /// Output directory, relative to the root document's directory.
    pub outdir: String,
    /// Builds running longer than this are killed.
    pub timeout_s: u64,
    /// The latexmk executable (looked up in `PATH` when relative).
    pub latexmk: PathBuf,
}

impl Default for BuildCfg {
    fn default() -> Self {
        Self {
            on_save: true,
            on_agent_turn_end: true,
            outdir: ".build".into(),
            timeout_s: 180,
            latexmk: "latexmk".into(),
        }
    }
}

/// `[pdf]`: the preview pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PdfCfg {
    /// Size cap of the rendered-page cache in MiB. A change applies to windows opened after it
    /// (the other `[pdf]` keys apply at once when config.toml is reloaded).
    pub cache_mib: u32,
    /// Invert page colours while the system style is dark.
    pub dark_invert: bool,
    /// Forward-sync to the cursor line while typing.
    pub follow_cursor: bool,
    /// Show the strip of page thumbnails and bookmarks at the left of the PDF preview (M3; the
    /// button in the preview's toolbar toggles it for the session).
    pub thumbnails: bool,
}

impl Default for PdfCfg {
    fn default() -> Self {
        Self {
            cache_mib: 48,
            dark_invert: true,
            follow_cursor: false,
            thumbnails: false,
        }
    }
}

/// `[agent]`: the ACP agent (M2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentCfg {
    /// Absolute path of `node`; empty means "auto-detect once and store".
    pub node: PathBuf,
    /// Absolute path of `claude`; empty means "auto-detect once and store".
    pub claude: PathBuf,
    /// npm prefix of the ACP adapter install (`~` is expanded with [`expand_tilde`]).
    pub adapter_dir: PathBuf,
    /// Adapter version installed by the in-app action.
    pub adapter_version: String,
    /// Save dirty project buffers before each prompt.
    pub save_before_prompt: bool,
    /// Cap of the per-turn snapshot of closed project files, in MiB.
    pub snapshot_cap_mib: u32,
    /// Stop an idle agent after this many minutes (M5).
    pub idle_stop_min: u32,
    /// Send a desktop notification when a turn ends while the window is not focused.
    pub notify_on_turn_end: bool,
    /// The session mode chosen when the agent starts: `"default"` or `"plan"` (the only modes
    /// Lemma offers; anything else is treated as `"default"`).
    pub default_mode: String,
    /// The `model` config option to select when the agent starts (an option value such as
    /// `"opus"`); empty keeps the agent's default.
    pub model: String,
    /// The `effort` config option to select when the agent starts (`"low"` … `"max"`); empty
    /// keeps the agent's default.
    pub effort: String,
    /// Which agent the pane talks to (M5): Claude Code through its ACP adapter, the Anthropic
    /// API with the user's key, or Pi through its ACP adapter.
    pub backend: AgentBackend,
    /// npm prefix of the Pi ACP adapter install (`~` is expanded with [`expand_tilde`]).
    pub pi_adapter_dir: PathBuf,
    /// Pi ACP adapter version (`@openma/pi-acp`) installed by the in-app action.
    pub pi_adapter_version: String,
}

/// `[agent] backend`: the agent behind the pane (M5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentBackend {
    /// Claude Code through `@agentclientprotocol/claude-agent-acp`.
    #[default]
    Claude,
    /// The Anthropic Messages API with the user's API key.
    Api,
    /// The pi coding agent through `@openma/pi-acp`.
    Pi,
}

impl AgentBackend {
    /// Every backend, in the order the picker lists them.
    pub const ALL: [AgentBackend; 3] = [Self::Claude, Self::Api, Self::Pi];

    /// The value in `config.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Api => "api",
            Self::Pi => "pi",
        }
    }

    /// The backend named `value` in `config.toml`.
    pub fn from_name(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.as_str() == value)
    }
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            node: PathBuf::new(),
            claude: PathBuf::new(),
            adapter_dir: "~/.local/share/lemma/acp".into(),
            adapter_version: "0.89.1".into(),
            save_before_prompt: true,
            snapshot_cap_mib: 4,
            idle_stop_min: 10,
            notify_on_turn_end: true,
            default_mode: "default".into(),
            model: String::new(),
            effort: String::new(),
            backend: AgentBackend::Claude,
            pi_adapter_dir: "~/.local/share/lemma/acp-pi".into(),
            pi_adapter_version: "0.1.7".into(),
        }
    }
}

/// `[spell]`: libspelling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpellCfg {
    pub enabled: bool,
    /// Enchant language tag.
    pub language: String,
    /// Plain word lists (one word per line) whose words are never flagged; `~` is expanded.
    pub word_lists: Vec<PathBuf>,
}

impl Default for SpellCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            language: "en_US".into(),
            word_lists: Vec::new(),
        }
    }
}

/// `[typing]`: typing helpers (§5 "Pairing", "Snippets"); each applies in Vim insert mode and
/// in plain editing, never to pastes, undo/redo, agent or review edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypingCfg {
    /// Close `{`, `[`, `(` (in math also `\{` and `\left(`), type over closing characters, and
    /// delete an empty pair with Backspace.
    pub autopair: bool,
    /// `$` pairs to `$$` in text, types over a closing `$`, never pairs after `\`.
    pub smart_dollar: bool,
    /// Insert `\end{x}` when `}` completes an unclosed `\begin{x}`, or on Enter after it.
    pub auto_end: bool,
    /// Enter in itemize/enumerate/description continues with `\item `; Enter on an empty item
    /// leaves the list.
    pub item_on_enter: bool,
    /// Expand automatic snippets (`ff` → `\frac{}{}` in math, …) as they are typed.
    pub autosnippets: bool,
    /// Typing replacements such as `...` → `\dots` (QoL P2).
    pub replacements: bool,
}

impl Default for TypingCfg {
    fn default() -> Self {
        Self {
            autopair: true,
            smart_dollar: true,
            auto_end: true,
            item_on_enter: true,
            autosnippets: true,
            replacements: false,
        }
    }
}

/// `[completion]`: the completion popup (§5 "Completion").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompletionCfg {
    /// Open the popup by itself while typing a word in text (commands, references and
    /// citations always open it).
    pub auto: bool,
    /// Shortest word, in characters, that opens the popup by itself.
    pub min_word: u32,
    /// Most proposals shown.
    pub max_items: u32,
}

impl Default for CompletionCfg {
    fn default() -> Self {
        Self {
            auto: true,
            min_word: 2,
            max_items: 50,
        }
    }
}

/// The Vim normal-mode key sequences of [`VimCfg::normal`] and the actions they run.
///
/// `gd` (go to the definition of the reference, citation, file or macro under the cursor) and
/// `K` (its hover popover at the cursor) are M3's.
pub const DEFAULT_VIM_NORMAL: &[(&str, &str)] = &[
    ("]h", "win.review-next"),
    ("[h", "win.review-prev"),
    ("ga", "win.review-accept"),
    ("gr", "win.review-reject"),
    ("<Space>a", "win.agent-focus"),
    ("gd", "win.goto-definition"),
    ("K", "win.show-hover"),
];

/// `[vim]`: Lemma's own Vim normal-mode mappings, on top of GtkSourceView's Vim emulation.
///
/// ```toml
/// [vim.normal]
/// "<Space>a" = "win.agent-focus"   # key sequence = detailed action name
/// "gr" = ""                        # an empty string removes a default mapping
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VimCfg {
    /// Overrides of [`DEFAULT_VIM_NORMAL`]: key sequence (Vim notation, `<Space>`, `<Leader>`
    /// is not special) → action name; use [`VimCfg::normal_bindings`] for the effective map.
    pub normal: BTreeMap<String, String>,
}

impl VimCfg {
    /// The effective normal-mode mappings: [`DEFAULT_VIM_NORMAL`] with the overrides applied
    /// (an empty action removes a mapping).
    pub fn normal_bindings(&self) -> BTreeMap<String, String> {
        let mut map: BTreeMap<String, String> = DEFAULT_VIM_NORMAL
            .iter()
            .map(|(k, a)| ((*k).to_owned(), (*a).to_owned()))
            .collect();
        for (keys, action) in &self.normal {
            if action.is_empty() {
                map.remove(keys);
            } else {
                map.insert(keys.clone(), action.clone());
            }
        }
        map
    }
}

/// `[history]`: local history, content-addressed snapshots of project files taken on save and
/// before every agent turn (QoL P1), kept in `state_dir/history/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryCfg {
    pub enabled: bool,
    /// Size cap of a project's history, in MiB; the oldest entries go first.
    pub max_mib: u32,
    /// Entries older than this many days are deleted.
    pub max_days: u32,
    /// At most one save entry per file in this many seconds.
    pub min_interval_s: u32,
}

impl Default for HistoryCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            max_mib: 200,
            max_days: 30,
            min_interval_s: 60,
        }
    }
}

/// `[outline]`: the Outline page of the sidebar (M3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutlineCfg {
    /// Select the row of the section the cursor is in (the editor is never scrolled by it).
    pub follow_cursor: bool,
}

impl Default for OutlineCfg {
    fn default() -> Self {
        Self {
            follow_cursor: true,
        }
    }
}

/// `[preview]`: hover popovers and the rendered math preview (M3, ARCHITECTURE §5 "Math hover
/// preview").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewCfg {
    /// Render the formula under the pointer in a hover popover.
    pub math_hover: bool,
    /// How long the pointer must rest before any hover popover appears, in milliseconds.
    pub delay_ms: u32,
    /// Rendered formulas kept in memory (an LRU cache).
    pub cache: u32,
    /// While the cursor is inside math (inline or display) in insert mode, render the formula
    /// in a small card below it after a pause in typing.
    pub live: bool,
    /// Environments that are not math for the scanner but are previewed as a whole, like math
    /// (`tikzcd` diagrams).
    pub envs: Vec<String>,
}

impl Default for PreviewCfg {
    fn default() -> Self {
        Self {
            math_hover: true,
            delay_ms: 300,
            cache: 64,
            live: true,
            envs: vec!["tikzcd".into()],
        }
    }
}

/// `[lint]`: chktex warnings in the Problems page (M3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LintCfg {
    /// Run chktex after saves (quietly off when chktex is not installed).
    pub chktex: bool,
    /// chktex warning numbers never shown.
    pub disabled: Vec<u32>,
    /// chktex warning numbers shown even though Lemma silences them by default (the noisiest
    /// ones).
    pub enabled: Vec<u32>,
}

impl Default for LintCfg {
    fn default() -> Self {
        Self {
            chktex: true,
            disabled: Vec::new(),
            enabled: Vec::new(),
        }
    }
}

/// `[status]`: optional status-bar items (M3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusCfg {
    /// The word count of the document (texcount after each successful build).
    pub word_count: bool,
}

impl Default for StatusCfg {
    fn default() -> Self {
        Self { word_count: true }
    }
}

/// `[git]`: git integration (M3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitCfg {
    /// Mark lines added, changed and deleted relative to `HEAD` in a thin gutter bar.
    pub gutter: bool,
}

impl Default for GitCfg {
    fn default() -> Self {
        Self { gutter: true }
    }
}

/// `[session]`: the open tabs, positions and panel layout of each project (M3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionCfg {
    /// Save the session of a project and restore it when a file of that project is opened.
    pub restore: bool,
}

impl Default for SessionCfg {
    fn default() -> Self {
        Self { restore: true }
    }
}

/// `[keys]`: accelerator overrides, written directly in the section:
///
/// ```toml
/// [keys]
/// "win.save" = "<Control>s"
/// "win.toggle-pdf" = ""        # an empty string removes the default binding
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeysCfg {
    /// Detailed action name (`win.save`, `app.quit`, …) → GTK accelerator string.
    pub bindings: BTreeMap<String, String>,
}

/// Parses the text of `config.toml`. Missing sections and keys take their defaults.
///
/// The error is a human-readable message that includes the location of the problem.
pub fn parse(src: &str) -> Result<Config, String> {
    toml::from_str(src).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_contract() {
        let c = Config::default();
        assert!(!c.editor.vim);
        assert_eq!(c.editor.renderer, Renderer::Auto);
        assert_eq!(c.editor.font, "Monospace 11");
        assert_eq!(c.editor.scheme_light, "Adwaita");
        assert_eq!(c.editor.scheme_dark, "Adwaita-dark");
        assert!(c.editor.math_envs.is_empty());
        assert!(c.editor.wrap);

        assert!(c.build.on_save);
        assert!(c.build.on_agent_turn_end);
        assert_eq!(c.build.outdir, ".build");
        assert_eq!(c.build.timeout_s, 180);
        assert_eq!(c.build.latexmk, PathBuf::from("latexmk"));

        assert_eq!(c.pdf.cache_mib, 48);
        assert!(c.pdf.dark_invert);
        assert!(!c.pdf.follow_cursor);

        assert_eq!(c.agent.node, PathBuf::new());
        assert_eq!(c.agent.claude, PathBuf::new());
        assert_eq!(
            c.agent.adapter_dir,
            PathBuf::from("~/.local/share/lemma/acp")
        );
        assert_eq!(c.agent.adapter_version, "0.89.1");
        assert!(c.agent.save_before_prompt);
        assert_eq!(c.agent.snapshot_cap_mib, 4);
        assert_eq!(c.agent.idle_stop_min, 10);

        assert!(c.spell.enabled);
        assert_eq!(c.spell.language, "en_US");
        assert!(c.spell.word_lists.is_empty());
        assert!(c.keys.bindings.is_empty());
    }

    #[test]
    fn m2_defaults() {
        let c = Config::default();
        assert_eq!(c.editor.math_tint, 6);

        let t = &c.typing;
        assert!(t.autopair && t.smart_dollar && t.auto_end && t.item_on_enter && t.autosnippets);
        assert!(!t.replacements);

        assert!(c.completion.auto);
        assert_eq!(c.completion.min_word, 2);
        assert_eq!(c.completion.max_items, 50);

        assert!(c.agent.notify_on_turn_end);
        assert_eq!(c.agent.default_mode, "default");
        assert_eq!(c.agent.model, "");
        assert_eq!(c.agent.effort, "");

        assert!(c.vim.normal.is_empty());
        let vim = c.vim.normal_bindings();
        assert_eq!(vim.len(), DEFAULT_VIM_NORMAL.len());
        assert_eq!(vim["]h"], "win.review-next");
        assert_eq!(vim["[h"], "win.review-prev");
        assert_eq!(vim["ga"], "win.review-accept");
        assert_eq!(vim["gr"], "win.review-reject");
        assert_eq!(vim["<Space>a"], "win.agent-focus");

        assert!(c.history.enabled);
        assert_eq!(c.history.max_mib, 200);
        assert_eq!(c.history.max_days, 30);
        assert_eq!(c.history.min_interval_s, 60);
    }

    #[test]
    fn m2_sections_parse_and_keep_their_defaults() {
        let c = parse(
            "[typing]\nautopair = false\nreplacements = true\n\
             [completion]\nmin_word = 3\n\
             [agent]\nmodel = \"opus\"\nnotify_on_turn_end = false\n\
             [editor]\nmath_tint = 0\n\
             [history]\nmax_days = 7\n",
        )
        .unwrap();
        assert!(!c.typing.autopair);
        assert!(c.typing.replacements);
        assert!(c.typing.smart_dollar, "unset keys keep their default");
        assert_eq!(c.completion.min_word, 3);
        assert_eq!(c.completion.max_items, 50);
        assert_eq!(c.agent.model, "opus");
        assert_eq!(c.agent.effort, "");
        assert_eq!(c.agent.default_mode, "default");
        assert!(!c.agent.notify_on_turn_end);
        assert_eq!(c.agent.adapter_version, "0.89.1");
        assert_eq!(c.editor.math_tint, 0);
        assert_eq!(c.history.max_days, 7);
        assert_eq!(c.history.max_mib, 200);
        assert!(
            parse("[typing]\nautopiar = true\n").is_err(),
            "typos are reported"
        );
        assert!(
            parse("[editor]\nmath_tint = 300\n").is_err(),
            "math_tint is a percentage"
        );
    }

    #[test]
    fn m3_defaults() {
        let c = Config::default();
        assert!(c.outline.follow_cursor);
        assert!(c.preview.math_hover);
        assert_eq!(c.preview.delay_ms, 300);
        assert_eq!(c.preview.cache, 64);
        assert!(c.preview.live);
        assert_eq!(c.preview.envs, ["tikzcd"]);
        assert!(c.lint.chktex);
        assert!(c.lint.disabled.is_empty() && c.lint.enabled.is_empty());
        assert!(c.status.word_count);
        assert!(c.git.gutter);
        assert!(c.session.restore);
        assert!(!c.pdf.thumbnails);
        assert!(c.editor.equation_numbers);
        assert!(c.editor.hover);
        let vim = c.vim.normal_bindings();
        assert_eq!(vim["gd"], "win.goto-definition");
        assert_eq!(vim["K"], "win.show-hover");
    }

    #[test]
    fn m3_sections_parse_and_keep_their_defaults() {
        let c = parse(
            "[outline]\nfollow_cursor = false\n\
             [preview]\ndelay_ms = 500\nenvs = [\"tikzcd\", \"tikzpicture\"]\n\
             [lint]\ndisabled = [1, 8]\n\
             [status]\nword_count = false\n\
             [git]\ngutter = false\n\
             [session]\nrestore = false\n\
             [pdf]\nthumbnails = true\n\
             [editor]\nhover = false\n",
        )
        .unwrap();
        assert!(!c.outline.follow_cursor);
        assert_eq!(c.preview.delay_ms, 500);
        assert_eq!(c.preview.cache, 64, "unset keys keep their default");
        assert!(c.preview.math_hover);
        assert_eq!(c.preview.envs, ["tikzcd", "tikzpicture"]);
        assert_eq!(c.lint.disabled, [1, 8]);
        assert!(c.lint.chktex);
        assert!(!c.status.word_count);
        assert!(!c.git.gutter);
        assert!(!c.session.restore);
        assert!(c.pdf.thumbnails);
        assert_eq!(c.pdf.cache_mib, 48);
        assert!(!c.editor.hover);
        assert!(c.editor.equation_numbers);
        assert!(
            parse("[preview]\ndelay = 1\n").is_err(),
            "typos are reported"
        );
        assert!(
            parse("[lint]\ndisabled = [-1]\n").is_err(),
            "warning numbers"
        );
        assert!(parse("[sesion]\nrestore = true\n").is_err());
    }

    #[test]
    fn vim_mappings_merge_over_the_defaults() {
        let c = parse(
            "[vim.normal]\n\"gr\" = \"\"\n\"<Space>a\" = \"win.toggle-agent\"\n\
             \"<Space>b\" = \"win.build\"\n",
        )
        .unwrap();
        let vim = c.vim.normal_bindings();
        assert!(
            !vim.contains_key("gr"),
            "an empty action removes the default"
        );
        assert_eq!(vim["<Space>a"], "win.toggle-agent");
        assert_eq!(vim["<Space>b"], "win.build");
        assert_eq!(vim["ga"], "win.review-accept");
        assert_eq!(vim.len(), DEFAULT_VIM_NORMAL.len());
    }

    #[test]
    fn empty_and_blank_files_give_the_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
        assert_eq!(parse("\n# only a comment\n\n").unwrap(), Config::default());
    }

    #[test]
    fn renderer_values() {
        for (text, renderer, gsk) in [
            ("auto", Renderer::Auto, "cairo"),
            ("cairo", Renderer::Cairo, "cairo"),
            ("vulkan", Renderer::Vulkan, "vulkan"),
            ("gl", Renderer::Gl, "ngl"),
        ] {
            let c = parse(&format!("[editor]\nrenderer = \"{text}\"\n")).unwrap();
            assert_eq!(c.editor.renderer, renderer);
            assert_eq!(renderer.gsk_name(), gsk);
        }
        assert!(parse("[editor]\nrenderer = \"ngl\"\n").is_err());
        assert!(parse("[editor]\nrenderer = \"Cairo\"\n").is_err());
    }

    #[test]
    fn autosave_values() {
        assert_eq!(Config::default().editor.autosave, Autosave::Off);
        for (text, autosave) in [
            ("off", Autosave::Off),
            ("focus", Autosave::Focus),
            ("idle", Autosave::Idle),
        ] {
            let c = parse(&format!("[editor]\nautosave = \"{text}\"\n")).unwrap();
            assert_eq!(c.editor.autosave, autosave);
        }
        assert!(parse("[editor]\nautosave = true\n").is_err());
        assert!(parse("[editor]\nautosave = \"always\"\n").is_err());
    }

    #[test]
    fn agent_backend_values() {
        let c = Config::default();
        assert_eq!(c.agent.backend, AgentBackend::Claude);
        assert_eq!(
            c.agent.pi_adapter_dir,
            PathBuf::from("~/.local/share/lemma/acp-pi")
        );
        assert_eq!(c.agent.pi_adapter_version, "0.1.7");
        for backend in AgentBackend::ALL {
            let c = parse(&format!("[agent]\nbackend = \"{}\"\n", backend.as_str())).unwrap();
            assert_eq!(c.agent.backend, backend);
            assert_eq!(AgentBackend::from_name(backend.as_str()), Some(backend));
        }
        assert!(parse("[agent]\nbackend = \"Pi\"\n").is_err());
        assert!(parse("[agent]\nbackend = \"openai\"\n").is_err());
        assert_eq!(AgentBackend::from_name("openai"), None);
    }

    #[test]
    fn partial_sections_keep_the_other_defaults() {
        let c =
            parse("[editor]\nvim = true\nfont = \"JetBrains Mono 12\"\n[pdf]\ncache_mib = 96\n")
                .unwrap();
        assert!(c.editor.vim);
        assert_eq!(c.editor.font, "JetBrains Mono 12");
        assert_eq!(c.editor.scheme_dark, "Adwaita-dark");
        assert!(c.editor.wrap);
        assert_eq!(c.pdf.cache_mib, 96);
        assert!(c.pdf.dark_invert);
        assert_eq!(c.build, BuildCfg::default());
        assert_eq!(c.agent, AgentCfg::default());
    }

    #[test]
    fn literal_strings_keep_backslashes_and_lists_parse() {
        let c = parse("[editor]\nmath_envs = ['dmath', 'mathpar*']\n[spell]\nword_lists = []\n")
            .unwrap();
        assert_eq!(c.editor.math_envs, ["dmath", "mathpar*"]);
        assert!(c.spell.word_lists.is_empty());
        assert!(c.spell.enabled);
    }

    #[test]
    fn keys_section_is_a_plain_map() {
        let c = parse("[keys]\n\"win.build\" = \"<Control>b\"\n\"app.quit\" = \"\"\n").unwrap();
        assert_eq!(c.keys.bindings.len(), 2);
        assert_eq!(c.keys.bindings["win.build"], "<Control>b");
        assert_eq!(c.keys.bindings["app.quit"], "");
    }

    #[test]
    fn unknown_sections_and_keys_are_errors() {
        let e = parse("[editr]\nvim = false\n").unwrap_err();
        assert!(e.contains("editr"), "{e}");
        let e = parse("[editor]\nvmi = false\n").unwrap_err();
        assert!(e.contains("vmi"), "{e}");
    }

    #[test]
    fn wrong_types_are_errors_with_a_location() {
        let e = parse("[build]\ntimeout_s = \"long\"\n").unwrap_err();
        assert!(e.contains("timeout_s"), "{e}");
        assert!(parse("[editor]\nwrap = 1\n").is_err());
        assert!(parse("not toml at all [").is_err());
    }

    #[test]
    fn serialized_config_parses_back() {
        let mut c = Config::default();
        c.editor.vim = false;
        c.keys
            .bindings
            .insert("win.save".into(), "<Control>s".into());
        c.agent.claude = "/opt/claude/bin/claude".into();
        c.vim.normal.insert("<Space>a".into(), String::new());
        c.typing.replacements = true;
        c.preview.envs.push("tikzpicture".into());
        c.lint.disabled = vec![1, 44];
        c.session.restore = false;
        let text = toml::to_string(&c).unwrap();
        assert_eq!(parse(&text).unwrap(), c);
    }
}
