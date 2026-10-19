use std::ffi::OsString;
use std::path::{Path, PathBuf};

const APP_DIR: &str = "lemma";

/// Lemma's per-user directories, each already ending in `lemma/` (ARCHITECTURE §10.1).
///
/// | Field | Default |
/// |---|---|
/// | `config_dir` | `$XDG_CONFIG_HOME/lemma`, else `~/.config/lemma` |
/// | `state_dir` | `$XDG_STATE_HOME/lemma`, else `~/.local/state/lemma` |
/// | `data_dir` | `$XDG_DATA_HOME/lemma`, else `~/.local/share/lemma` |
/// | `runtime_dir` | `$XDG_RUNTIME_DIR/lemma`, else `cache_dir/run` |
/// | `cache_dir` | `$XDG_CACHE_HOME/lemma`, else `~/.cache/lemma` |
///
/// As the XDG base-directory spec requires, relative `XDG_*` values are ignored. Nothing is
/// created on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// The user's home directory, used to expand `~` in configured paths.
    pub home: PathBuf,
}

impl Paths {
    /// Computes the paths from the process environment. `None` when `HOME` is unset or relative.
    pub fn from_env() -> Option<Self> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// Computes the paths from `var`, a lookup of environment variables (`HOME`, `XDG_*`).
    /// `None` when `HOME` is missing or not absolute.
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        let home = var("HOME").map(PathBuf::from).filter(|p| p.is_absolute())?;
        let base = |name: &str, fallback: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
                .join(APP_DIR)
        };
        let cache_dir = base("XDG_CACHE_HOME", ".cache");
        let runtime_dir = var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map_or_else(|| cache_dir.join("run"), |p| p.join(APP_DIR));
        Some(Self {
            config_dir: base("XDG_CONFIG_HOME", ".config"),
            state_dir: base("XDG_STATE_HOME", ".local/state"),
            data_dir: base("XDG_DATA_HOME", ".local/share"),
            runtime_dir,
            cache_dir,
            home,
        })
    }

    /// `config_dir/config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// `config_dir/snippets.toml` (user snippets, merged over the defaults).
    pub fn snippets_file(&self) -> PathBuf {
        self.config_dir.join("snippets.toml")
    }

    /// `config_dir/prompts.toml` (user quick actions, merged over the defaults).
    pub fn prompts_file(&self) -> PathBuf {
        self.config_dir.join("prompts.toml")
    }

    /// `state_dir/history/` (local history, one directory per project).
    pub fn history_dir(&self) -> PathBuf {
        self.state_dir.join("history")
    }

    /// `state_dir/projects.toml` (remembered root documents).
    pub fn projects_file(&self) -> PathBuf {
        self.state_dir.join("projects.toml")
    }

    /// `state_dir/review/` (persisted agent-edit reviews).
    pub fn review_dir(&self) -> PathBuf {
        self.state_dir.join("review")
    }

    /// `state_dir/sessions/` (M3: one `<fnv(root)>.json` per project: open tabs, positions,
    /// panel layout).
    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    /// `cache_dir/preview/` (M3: the math preview's preamble formats, one directory per
    /// project root).
    pub fn preview_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("preview")
    }

    /// Expands a leading `~` in `path` with this user's home directory.
    pub fn expand(&self, path: &Path) -> PathBuf {
        expand_tilde(path, &self.home)
    }
}

/// Replaces a leading `~` component of `path` with `home`; any other path is returned unchanged
/// (`~user/…` is not supported and stays as it is).
pub fn expand_tilde(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn defaults_hang_off_home() {
        let p = Paths::from_vars(vars(&[("HOME", "/home/u")])).unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/home/u/.config/lemma"));
        assert_eq!(p.state_dir, PathBuf::from("/home/u/.local/state/lemma"));
        assert_eq!(p.data_dir, PathBuf::from("/home/u/.local/share/lemma"));
        assert_eq!(p.cache_dir, PathBuf::from("/home/u/.cache/lemma"));
        assert_eq!(p.runtime_dir, PathBuf::from("/home/u/.cache/lemma/run"));
        assert_eq!(p.home, PathBuf::from("/home/u"));
        assert_eq!(
            p.config_file(),
            PathBuf::from("/home/u/.config/lemma/config.toml")
        );
        assert_eq!(
            p.snippets_file(),
            PathBuf::from("/home/u/.config/lemma/snippets.toml")
        );
        assert_eq!(
            p.projects_file(),
            PathBuf::from("/home/u/.local/state/lemma/projects.toml")
        );
        assert_eq!(
            p.review_dir(),
            PathBuf::from("/home/u/.local/state/lemma/review")
        );
        assert_eq!(
            p.prompts_file(),
            PathBuf::from("/home/u/.config/lemma/prompts.toml")
        );
        assert_eq!(
            p.history_dir(),
            PathBuf::from("/home/u/.local/state/lemma/history")
        );
        assert_eq!(
            p.sessions_dir(),
            PathBuf::from("/home/u/.local/state/lemma/sessions")
        );
        assert_eq!(
            p.preview_cache_dir(),
            PathBuf::from("/home/u/.cache/lemma/preview")
        );
    }

    #[test]
    fn xdg_variables_win_when_absolute() {
        let p = Paths::from_vars(vars(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "/st"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_CACHE_HOME", "/cache"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]))
        .unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/cfg/lemma"));
        assert_eq!(p.state_dir, PathBuf::from("/st/lemma"));
        assert_eq!(p.data_dir, PathBuf::from("/data/lemma"));
        assert_eq!(p.cache_dir, PathBuf::from("/cache/lemma"));
        assert_eq!(p.runtime_dir, PathBuf::from("/run/user/1000/lemma"));
    }

    #[test]
    fn relative_xdg_values_are_ignored() {
        let p = Paths::from_vars(vars(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "relative/cfg"),
            ("XDG_RUNTIME_DIR", ""),
        ]))
        .unwrap();
        assert_eq!(p.config_dir, PathBuf::from("/home/u/.config/lemma"));
        assert_eq!(p.runtime_dir, PathBuf::from("/home/u/.cache/lemma/run"));
    }

    #[test]
    fn missing_or_relative_home_gives_none() {
        assert!(Paths::from_vars(vars(&[])).is_none());
        assert!(Paths::from_vars(vars(&[("HOME", "home/u")])).is_none());
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(
            expand_tilde(Path::new("~/.config/nvim/x.txt"), home),
            PathBuf::from("/home/u/.config/nvim/x.txt")
        );
        assert_eq!(expand_tilde(Path::new("~"), home), PathBuf::from("/home/u"));
        assert_eq!(
            expand_tilde(Path::new("/abs/~/x"), home),
            PathBuf::from("/abs/~/x")
        );
        assert_eq!(
            expand_tilde(Path::new("~other/x"), home),
            PathBuf::from("~other/x")
        );
        assert_eq!(
            expand_tilde(Path::new("rel/x"), home),
            PathBuf::from("rel/x")
        );
        let p = Paths::from_vars(vars(&[("HOME", "/home/u")])).unwrap();
        assert_eq!(
            p.expand(&crate::AgentCfg::default().adapter_dir),
            PathBuf::from("/home/u/.local/share/lemma/acp")
        );
    }
}
