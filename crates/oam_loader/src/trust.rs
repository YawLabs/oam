//! Package trust management for lifecycle scripts.
//!
//! By default, oam install does not run lifecycle scripts (postinstall,
//! preinstall, install). A package must appear in the trust list before its
//! scripts are allowed to execute.
//!
//! Two tiers are merged at install time:
//! - Global: `$XDG_CONFIG_HOME/oam/trust.json` (or `~/.config/oam/trust.json`)
//! - Project-local: `.oam/trust.json` at the repo root (committed, team-shared)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The trust config stored on disk.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct TrustConfig {
    /// Trusted npm package names (e.g. `"esbuild"`, `"@scope/pkg"`).
    #[serde(default)]
    pub packages: Vec<String>,
}

impl TrustConfig {
    /// Load and merge project-local (`.oam/trust.json`) with global config.
    pub fn load(project_dir: &Path) -> Self {
        let mut merged = Self::load_global();
        let local = Self::load_local(project_dir);
        merged.packages.extend(local.packages);
        merged
    }

    /// Load only the global config (`~/.config/oam/trust.json`).
    pub fn load_global() -> Self {
        global_trust_path()
            .and_then(|p| read_config(&p))
            .unwrap_or_default()
    }

    /// Load only the project-local config (`.oam/trust.json`).
    pub fn load_local(project_dir: &Path) -> Self {
        let path = project_dir.join(".oam").join("trust.json");
        read_config(&path).unwrap_or_default()
    }

    /// Return `true` if the package name is in the trust list.
    pub fn is_trusted(&self, name: &str) -> bool {
        self.packages.iter().any(|p| p == name)
    }

    /// Add a package name. Returns `true` if actually added (not already present).
    pub fn add(&mut self, name: &str) -> bool {
        if self.is_trusted(name) {
            return false;
        }
        self.packages.push(name.to_string());
        true
    }

    /// Remove all entries matching the package name. Returns `true` if any were removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.packages.len();
        self.packages.retain(|p| p != name);
        self.packages.len() < before
    }

    /// Save to the project-local path (`.oam/trust.json`).
    pub fn save_local(&self, project_dir: &Path) -> std::io::Result<()> {
        let path = project_dir.join(".oam").join("trust.json");
        save_config(self, &path)
    }

    /// Save to the global path (`~/.config/oam/trust.json`).
    pub fn save_global(&self) -> std::io::Result<()> {
        let path = global_trust_path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not determine home directory",
            )
        })?;
        save_config(self, &path)
    }

    /// Return all entries (for `oam trust list`).
    pub fn entries(&self) -> &[String] {
        &self.packages
    }

    /// Return the path where global trust is stored (for display).
    pub fn global_path() -> Option<PathBuf> {
        global_trust_path()
    }
}

fn global_trust_path() -> Option<PathBuf> {
    fn process_env(key: &str) -> Option<std::ffi::OsString> {
        std::env::var_os(key)
    }
    global_trust_path_from(process_env)
}

/// The global trust file for an environment read through `var`:
/// `$XDG_CONFIG_HOME/oam/trust.json`, else `<home>/.config/oam/trust.json`
/// with `<home>` from `HOME`, then `USERPROFILE`.
///
/// A variable that is EMPTY or not an absolute path counts as unset (the XDG
/// base-directory spec says to ignore such a value). Taken as written,
/// `XDG_CONFIG_HOME=` would make the "global" list `./oam/trust.json` --
/// read from, and saved into, whatever directory oam happens to run in.
fn global_trust_path_from(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute_dir = |key: &str| var(key).map(PathBuf::from).filter(|dir| dir.is_absolute());
    absolute_dir("XDG_CONFIG_HOME")
        .or_else(|| {
            absolute_dir("HOME")
                .or_else(|| absolute_dir("USERPROFILE"))
                .map(|home| home.join(".config"))
        })
        .map(|base| base.join("oam").join("trust.json"))
}

fn read_config(path: &Path) -> Option<TrustConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&content) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            // Queued, not printed: the loader never writes to stderr itself
            // (warnings.rs), so `oam install --json` stays JSONL. The CLI
            // drains the queue through its diagnostic renderer.
            crate::warnings::push(oam_diagnostics::Diagnostic::new(
                "OAM-PKG0007",
                oam_diagnostics::Severity::Warning,
                oam_diagnostics::Origin::Install,
                format!(
                    "trust config at {} is malformed ({e}); treating it as empty, so no lifecycle script is trusted through it",
                    path.display()
                ),
            ));
            None
        }
    }
}

fn save_config(config: &TrustConfig, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = serde_json::to_string_pretty(config).map_err(std::io::Error::other)?;
    content.push('\n');
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// An environment made of the given pairs, as `global_trust_path_from`
    /// reads one.
    fn env_of(pairs: &[(&'static str, PathBuf)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs = pairs.to_vec();
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone().into_os_string())
        }
    }

    #[test]
    fn global_trust_path_prefers_xdg_then_home_then_userprofile() {
        // Absolute on every platform (a drive-qualified path on Windows).
        let base = std::env::temp_dir();
        let xdg = base.join("xdg");
        let home = base.join("home");
        let profile = base.join("profile");
        assert_eq!(
            global_trust_path_from(env_of(&[
                ("XDG_CONFIG_HOME", xdg.clone()),
                ("HOME", home.clone()),
            ])),
            Some(xdg.join("oam").join("trust.json"))
        );
        assert_eq!(
            global_trust_path_from(env_of(&[
                ("HOME", home.clone()),
                ("USERPROFILE", profile.clone()),
            ])),
            Some(home.join(".config").join("oam").join("trust.json"))
        );
        assert_eq!(
            global_trust_path_from(env_of(&[("USERPROFILE", profile.clone())])),
            Some(profile.join(".config").join("oam").join("trust.json"))
        );
        assert_eq!(global_trust_path_from(env_of(&[])), None);
    }

    /// The global list is never a path relative to the working directory: an
    /// empty or relative variable counts as unset and the next source is used.
    #[test]
    fn empty_or_relative_config_dirs_count_as_unset() {
        let home = std::env::temp_dir().join("home");
        let via_home = Some(home.join(".config").join("oam").join("trust.json"));
        for unusable in ["", "relative/config", "."] {
            assert_eq!(
                global_trust_path_from(env_of(&[
                    ("XDG_CONFIG_HOME", PathBuf::from(unusable)),
                    ("HOME", home.clone()),
                ])),
                via_home,
                "XDG_CONFIG_HOME={unusable:?}"
            );
            // With nothing usable anywhere there is no global path at all,
            // rather than one under the current directory.
            assert_eq!(
                global_trust_path_from(env_of(&[
                    ("XDG_CONFIG_HOME", PathBuf::from(unusable)),
                    ("HOME", PathBuf::from(unusable)),
                    ("USERPROFILE", PathBuf::from(unusable)),
                ])),
                None,
                "{unusable:?}"
            );
            assert_eq!(
                global_trust_path_from(env_of(&[
                    ("HOME", PathBuf::from(unusable)),
                    ("USERPROFILE", home.clone()),
                ])),
                via_home,
                "HOME={unusable:?}"
            );
        }
    }
}
