//! The user's settings files and their live reload: `config.toml` ([`lemma_config::Config`]),
//! `snippets.toml` (merged over the default snippets) and `prompts.toml` (merged over the default
//! quick actions).
//!
//! The files live in `~/.config/lemma/`; with `LEMMA_CONFIG=path`, `config.toml` is `path` and
//! the other two are looked for next to it (so `LEMMA_CONFIG=/dev/null` means defaults for all
//! three, which golden tests rely on). [`load`] reads them once at start-up. A [`Watcher`] then
//! reloads each file 300 ms after its last change on disk (editors write in several steps); the
//! app hands the new values to every window (`WindowState::set_config`, `set_snippets`,
//! `set_prompts`), whose `connect_config_changed` callbacks follow.
//!
//! A file with errors is reported and leaves the previous values in place (at start-up: the
//! defaults).

use gio::prelude::*;
use lemma_config::{Config, EditorCfg, Paths, PromptTemplate};
use lemma_snippets::SnippetSet;
use lemma_syntax::ScanConfig;
use std::cell::RefCell;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// Quiet time after the last change of a settings file before it is read again.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

/// One of the settings files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfigFile {
    Config,
    Snippets,
    Prompts,
}

impl ConfigFile {
    pub const ALL: [ConfigFile; 3] = [Self::Config, Self::Snippets, Self::Prompts];
}

/// Where the settings files are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsFiles {
    pub config: PathBuf,
    pub snippets: PathBuf,
    pub prompts: PathBuf,
}

impl SettingsFiles {
    /// `$LEMMA_CONFIG` and its directory when set, else the files in `paths.config_dir`.
    /// `None` without either (no `HOME`).
    pub fn locate(paths: Option<&Paths>) -> Option<Self> {
        match std::env::var_os("LEMMA_CONFIG") {
            Some(config) => Some(Self::next_to(PathBuf::from(config))),
            None => paths.map(|p| Self {
                config: p.config_file(),
                snippets: p.snippets_file(),
                prompts: p.prompts_file(),
            }),
        }
    }

    /// `config` with `snippets.toml` and `prompts.toml` in its directory.
    pub fn next_to(config: PathBuf) -> Self {
        let dir = config.parent().map(Path::to_path_buf).unwrap_or_default();
        Self {
            snippets: dir.join("snippets.toml"),
            prompts: dir.join("prompts.toml"),
            config,
        }
    }

    /// The path of `file`.
    pub fn path(&self, file: ConfigFile) -> &Path {
        match file {
            ConfigFile::Config => &self.config,
            ConfigFile::Snippets => &self.snippets,
            ConfigFile::Prompts => &self.prompts,
        }
    }
}

/// The parsed settings.
#[derive(Clone)]
pub struct Settings {
    pub config: Rc<Config>,
    pub snippets: Rc<SnippetSet>,
    pub prompts: Rc<Vec<PromptTemplate>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            config: Rc::new(Config::default()),
            snippets: Rc::new(SnippetSet::defaults()),
            prompts: Rc::new(lemma_config::default_prompts()),
        }
    }
}

/// A newly read settings file.
pub enum Reloaded {
    Config(Rc<Config>),
    Snippets(Rc<SnippetSet>),
    Prompts(Rc<Vec<PromptTemplate>>),
}

/// Reads every settings file synchronously (before the main loop starts, so it cannot stall a
/// frame). Returns the settings, with defaults for missing or broken files, and a message per
/// broken file.
pub fn load(files: Option<&SettingsFiles>) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let mut errors = Vec::new();
    let Some(files) = files else {
        return (settings, errors);
    };
    for file in ConfigFile::ALL {
        let path = files.path(file);
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                errors.push(format!("cannot read {}: {e}", path.display()));
                continue;
            }
        };
        match parse(file, &text) {
            Ok(value) => settings.apply(value),
            Err(e) => errors.push(error_message(path, &e)),
        }
    }
    (settings, errors)
}

impl Settings {
    fn apply(&mut self, value: Reloaded) {
        match value {
            Reloaded::Config(c) => self.config = c,
            Reloaded::Snippets(s) => self.snippets = s,
            Reloaded::Prompts(p) => self.prompts = p,
        }
    }
}

/// Parses the text of `file` (merging snippets and prompts over their defaults).
pub fn parse(file: ConfigFile, text: &str) -> Result<Reloaded, String> {
    Ok(match file {
        ConfigFile::Config => Reloaded::Config(Rc::new(lemma_config::parse(text)?)),
        ConfigFile::Snippets => {
            let user = SnippetSet::from_toml(text).map_err(|e| e.to_string())?;
            Reloaded::Snippets(Rc::new(SnippetSet::merged(SnippetSet::defaults(), user)))
        }
        ConfigFile::Prompts => Reloaded::Prompts(Rc::new(lemma_config::merge_prompts(
            lemma_config::default_prompts(),
            lemma_config::parse_prompts(text)?,
        ))),
    })
}

/// The message for a settings file with errors.
pub fn error_message(path: &Path, error: &str) -> String {
    format!(
        "{} has errors, keeping the previous settings: {error}",
        path.display()
    )
}

/// Reads `file` again (asynchronously). A missing file means its defaults.
pub async fn reload(files: &SettingsFiles, file: ConfigFile) -> Result<Reloaded, String> {
    let path = files.path(file);
    let text = match gio::File::for_path(path).load_contents_future().await {
        Ok((bytes, _)) => String::from_utf8(bytes.to_vec())
            .map_err(|_| format!("{} is not UTF-8", path.display()))?,
        Err(e) if e.matches(gio::IOErrorEnum::NotFound) => String::new(),
        Err(e) => return Err(format!("cannot read {}: {}", path.display(), e.message())),
    };
    parse(file, &text).map_err(|e| error_message(path, &e))
}

type Timers = Rc<RefCell<Vec<(ConfigFile, glib::SourceId)>>>;

/// Watches the settings files; see [`Watcher::start`].
pub struct Watcher {
    monitors: Vec<gio::FileMonitor>,
    targets: Rc<Targets>,
}

/// The monitors of the files that settings files reach through symbolic links.
struct Targets {
    changed: Rc<dyn Fn(ConfigFile)>,
    timers: Timers,
    /// Per settings file: the resolved target and its monitor.
    watched: RefCell<Vec<(ConfigFile, PathBuf, gio::FileMonitor)>>,
}

impl Watcher {
    /// Calls `changed(file)` [`RELOAD_DEBOUNCE`] after the last change on disk of each settings
    /// file (created, changed, replaced or deleted). Files that exist but are not regular files
    /// (`/dev/null`) are not watched.
    ///
    /// A settings file reached through a symbolic link (a dotfiles repository, GNU stow) is
    /// watched at its target too: an editor writing through the link, or saving the target in
    /// its own directory, changes nothing next to the link. The target is resolved again
    /// whenever the link itself changes.
    pub fn start(files: &SettingsFiles, changed: impl Fn(ConfigFile) + 'static) -> Self {
        let targets = Rc::new(Targets {
            changed: Rc::new(changed),
            timers: Rc::default(),
            watched: RefCell::default(),
        });
        let mut monitors = Vec::new();
        for file in ConfigFile::ALL {
            let path = files.path(file);
            if path.exists() && !path.is_file() {
                continue;
            }
            let weak = Rc::downgrade(&targets);
            let link = path.to_path_buf();
            let Some(monitor) = monitor(path, move || {
                if let Some(targets) = weak.upgrade() {
                    // The link may now point elsewhere (or be a plain file again).
                    targets.update(file, &link);
                    targets.debounce(file);
                }
            }) else {
                continue;
            };
            monitors.push(monitor);
            targets.update(file, path);
        }
        Self { monitors, targets }
    }

    /// Number of files watched.
    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    /// True when no file is watched.
    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for monitor in &self.monitors {
            monitor.cancel();
        }
        for (_, _, monitor) in self.targets.watched.borrow_mut().drain(..) {
            monitor.cancel();
        }
        for (_, timer) in self.targets.timers.borrow_mut().drain(..) {
            timer.remove();
        }
    }
}

impl Targets {
    /// Watches the file that `path` (the settings file `file`) resolves to, when that is not
    /// `path` itself; stops watching a target it no longer resolves to.
    fn update(self: &Rc<Self>, file: ConfigFile, path: &Path) {
        let target = lemma_core::canonicalize(path)
            .ok()
            .filter(|t| t.is_file())
            .filter(|t| std::path::absolute(path).ok().as_ref() != Some(t));
        let mut watched = self.watched.borrow_mut();
        if let Some(i) = watched.iter().position(|(f, _, _)| *f == file) {
            if Some(&watched[i].1) == target.as_ref() {
                return;
            }
            let (_, _, old) = watched.remove(i);
            old.cancel();
        }
        let Some(target) = target else {
            return;
        };
        let weak = Rc::downgrade(self);
        if let Some(monitor) = monitor(&target, move || {
            if let Some(targets) = weak.upgrade() {
                targets.debounce(file);
            }
        }) {
            tracing::debug!("watching {} at {}", path.display(), target.display());
            watched.push((file, target, monitor));
        }
    }

    fn debounce(&self, file: ConfigFile) {
        debounce(&self.timers, file, self.changed.clone());
    }
}

/// A monitor of the file at `path` that calls `f` on each event that may change its contents.
fn monitor(path: &Path, f: impl Fn() + 'static) -> Option<gio::FileMonitor> {
    let monitor = match gio::File::for_path(path)
        .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
    {
        Ok(monitor) => monitor,
        Err(e) => {
            tracing::warn!("cannot watch {}: {}", path.display(), e.message());
            return None;
        }
    };
    monitor.connect_changed(move |_, _, _, event| {
        if !matches!(
            event,
            gio::FileMonitorEvent::AttributeChanged
                | gio::FileMonitorEvent::PreUnmount
                | gio::FileMonitorEvent::Unmounted
        ) {
            f();
        }
    });
    Some(monitor)
}

/// (Re)starts the debounce timer of `file`.
fn debounce(timers: &Timers, file: ConfigFile, changed: Rc<dyn Fn(ConfigFile)>) {
    let mut list = timers.borrow_mut();
    if let Some(i) = list.iter().position(|(f, _)| *f == file) {
        let (_, timer) = list.remove(i);
        timer.remove();
    }
    let weak = Rc::downgrade(timers);
    let id = glib::timeout_add_local_once(RELOAD_DEBOUNCE, move || {
        if let Some(timers) = weak.upgrade() {
            timers.borrow_mut().retain(|(f, _)| *f != file);
        }
        changed(file);
    });
    list.push((file, id));
}

/// The scanner rules for `cfg`: VimTeX's, plus the user's `math_envs`.
pub fn scan_config(cfg: &EditorCfg) -> Arc<ScanConfig> {
    let mut scan = ScanConfig::default();
    for env in &cfg.math_envs {
        let env = env.trim();
        if !env.is_empty() && !scan.math_envs.iter().any(|e| e == env) {
            scan.math_envs.push(env.to_owned());
        }
    }
    Arc::new(scan)
}

/// The live configuration of a window: dereferences to the current [`Config`], so
/// `state.config.build.on_save` always reads the latest values.
///
/// Every version is kept until the window goes, so a `&Config` taken before a reload stays
/// valid (reloads are rare and a `Config` is small).
pub struct LiveConfig {
    versions: RefCell<Vec<Rc<Config>>>,
}

impl LiveConfig {
    /// Starts with `config`.
    pub fn new(config: Rc<Config>) -> Self {
        Self {
            versions: RefCell::new(vec![config]),
        }
    }

    /// The current configuration, as a shareable handle.
    pub fn get(&self) -> Rc<Config> {
        self.versions
            .borrow()
            .last()
            .cloned()
            .expect("a LiveConfig always has a version")
    }

    /// Makes `config` the current configuration.
    pub fn set(&self, config: Rc<Config>) {
        self.versions.borrow_mut().push(config);
    }
}

impl Deref for LiveConfig {
    type Target = Config;

    fn deref(&self) -> &Config {
        let ptr = Rc::as_ptr(
            self.versions
                .borrow()
                .last()
                .expect("a LiveConfig always has a version"),
        );
        // SAFETY: `versions` only ever grows, so the `Rc` that owns `*ptr` stays in it (and the
        // `Config` it points to stays allocated and unchanged: nothing hands out mutable access)
        // until `self` is dropped, which the returned borrow of `self` cannot outlive.
        unsafe { &*ptr }
    }
}

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("versions", &self.versions.borrow().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_next_to_lemma_config() {
        let files = SettingsFiles::next_to(PathBuf::from("/tmp/x/my.toml"));
        assert_eq!(files.config, PathBuf::from("/tmp/x/my.toml"));
        assert_eq!(files.snippets, PathBuf::from("/tmp/x/snippets.toml"));
        assert_eq!(files.prompts, PathBuf::from("/tmp/x/prompts.toml"));
        let files = SettingsFiles::next_to(PathBuf::from("/dev/null"));
        assert_eq!(
            files.path(ConfigFile::Snippets),
            Path::new("/dev/snippets.toml")
        );
    }

    #[test]
    fn load_reads_merges_and_reports() {
        let dir = std::env::temp_dir().join(format!("lemma-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = SettingsFiles::next_to(dir.join("config.toml"));
        std::fs::write(&files.config, "[editor]\nvim = true\n").unwrap();
        std::fs::write(
            &files.snippets,
            "[[snippet]]\ntrigger = \"zz\"\nbody = 'zeta'\nauto = true\n",
        )
        .unwrap();
        std::fs::write(
            &files.prompts,
            "[[prompt]]\nname = \"Shorten\"\nprompt = \"p\"\n",
        )
        .unwrap();
        let (settings, errors) = load(Some(&files));
        assert!(errors.is_empty(), "{errors:?}");
        assert!(settings.config.editor.vim);
        assert_eq!(settings.snippets.len(), SnippetSet::defaults().len() + 1);
        assert_eq!(settings.prompts.len(), 7);
        assert_eq!(settings.prompts[6].name, "Shorten");

        std::fs::write(&files.config, "[editor]\nvim = 3\n").unwrap();
        std::fs::write(&files.prompts, "[[prompt]]\nname = 1\n").unwrap();
        let (settings, errors) = load(Some(&files));
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("config.toml has errors"), "{errors:?}");
        assert!(!settings.config.editor.vim, "defaults for a broken file");
        assert_eq!(settings.prompts.len(), 6);
        std::fs::remove_dir_all(&dir).unwrap();

        let (settings, errors) = load(Some(&SettingsFiles::next_to("/dev/null".into())));
        assert!(errors.is_empty());
        assert_eq!(*settings.config, Config::default());
        assert_eq!(settings.snippets.len(), SnippetSet::defaults().len());
    }

    #[test]
    fn scan_config_adds_the_users_math_environments() {
        let mut cfg = EditorCfg::default();
        assert_eq!(*scan_config(&cfg), ScanConfig::default());
        cfg.math_envs = vec!["mathpar".into(), "align".into(), " ".into()];
        let scan = scan_config(&cfg);
        assert_eq!(
            scan.math_envs.len(),
            ScanConfig::default().math_envs.len() + 1
        );
        assert_eq!(scan.math_envs.last().map(String::as_str), Some("mathpar"));
    }

    #[test]
    fn live_config_follows_sets_and_keeps_old_borrows_valid() {
        let live = LiveConfig::new(Rc::new(Config::default()));
        assert!(!live.editor.vim);
        let old: &Config = &live;
        let mut next = Config::default();
        next.editor.vim = true;
        live.set(Rc::new(next));
        assert!(live.editor.vim, "reads see the new version");
        assert!(
            !old.editor.vim,
            "a borrow taken before stays valid and unchanged"
        );
        assert!(live.get().editor.vim);
    }
}
