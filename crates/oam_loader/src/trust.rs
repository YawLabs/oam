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
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .map(|base| base.join("oam").join("trust.json"))
}

fn read_config(path: &Path) -> Option<TrustConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&content) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!(
                "oam: warning: trust config at {} is malformed ({}); treating as empty",
                path.display(),
                e
            );
            None
        }
    }
}

fn save_config(config: &TrustConfig, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = serde_json::to_string_pretty(config)
        .map_err(std::io::Error::other)?;
    content.push('\n');
    std::fs::write(path, content)
}
