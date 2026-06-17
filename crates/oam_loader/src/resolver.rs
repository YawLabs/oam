//! Per-resolver caches for the loader.
//!
//! The original `oam_loader` kept two process-globals (`negative_probe_cache`
//! in `lib.rs` and `tsconfig_cache` in `tsconfig.rs`) via `OnceLock<Mutex<...>>`.
//! That worked for short-lived CLI invocations (`oam run` is a fresh process)
//! but a longer-lived process -- a daemon, an LSP, a test harness that drives
//! many resolves, an MCP server -- can have the same `oam_loader` instance
//! resolve before and after a project layout change (e.g. `oam install` adds
//! a new `node_modules/foo` or rewrites `tsconfig.json`). The process-global
//! cache would return the stale "not found" / stale parsed config.
//!
//! `Resolver` owns the caches. Construct one per long-lived scope; call
//! `clear_caches()` after any project layout mutation. The free functions
//! (`resolve_import`, `transpile_typescript`, ...) keep their existing
//! signatures by routing through a thread-local default `Resolver` -- so
//! existing callers and tests are unaffected.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oam_diagnostics::{Diagnostic, Origin, Severity};

use crate::tsconfig::PathsConfig;

/// A loaded tsconfig entry: either a parsed `PathsConfig` (may be empty) or
/// the explicit `None` we cached for a tsconfig that had no `paths` field.
pub type TsconfigEntry = Option<PathsConfig>;

/// Owns the per-resolver caches that used to be process-globals.
#[derive(Default)]
pub struct Resolver {
    /// Paths confirmed NOT to exist on a recent resolve. Speeds up repeated
    /// misses for the same specifier across the same resolver scope.
    negative_probes: Mutex<HashSet<PathBuf>>,
    /// `tsconfig.json` path -> parsed paths config. None = file existed but
    /// had no paths (so we don't re-stat it).
    tsconfigs: Mutex<HashMap<PathBuf, TsconfigEntry>>,
}

impl Resolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cached entry. Call this when the project layout has
    /// changed (e.g. after `oam install`, after editing `tsconfig.json`).
    pub fn clear_caches(&self) {
        self.negative_probes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.tsconfigs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub(crate) fn tsconfigs(&self) -> &Mutex<HashMap<PathBuf, TsconfigEntry>> {
        &self.tsconfigs
    }

    /// Check whether `path` resolves to a file, consulting the negative
    /// cache first. Insert into the negative set on miss; remove on hit (a
    /// file may have been created since the last check).
    pub(crate) fn is_file_cached(&self, path: &Path) -> bool {
        {
            let neg = self
                .negative_probes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if neg.contains(path) {
                return false;
            }
        }
        if path.is_file() {
            self.negative_probes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(path);
            true
        } else {
            self.negative_probes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(path.to_path_buf());
            false
        }
    }

    /// Resolve an import specifier as written in the module at `referrer`.
    ///
    /// Relative + absolute paths: candidate order for './x' is exact (if it has
    /// an extension), TS-source fallback for JS extensions ('./x.js' -> x.ts,
    /// the tsgo rewrite convention), then extensionless probing (.ts, .mts, .js,
    /// .mjs) and directory index (index.ts, index.js).
    ///
    /// Bare specifiers resolve via tsconfig paths (if a tsconfig.json is found
    /// in the referrer's ancestor tree) then the Node ESM node_modules walk.
    ///
    /// `node:` / `oam:` specifiers resolve to virtual builtin paths.
    /// `node:`-prefixed builtins not yet in wave 1 surface OAM-MOD0006.
    pub fn resolve_import(
        &self,
        specifier: &str,
        referrer: &Path,
    ) -> Result<PathBuf, Diagnostic> {
        // Shapes that are invalid as ESM specifiers everywhere — npm resolution
        // will never fix these, so they get their own diagnostic, not MOD0002.
        if specifier.is_empty() || specifier == "." || specifier == ".." || specifier.contains('\\') {
            return Err(Diagnostic::new(
                "OAM-MOD0004",
                Severity::Error,
                Origin::Resolve,
                format!(
                    "invalid module specifier '{specifier}' in {}: use './name' / '../name' with forward slashes",
                    referrer.display()
                ),
            ));
        }

        let is_relative = specifier.starts_with("./") || specifier.starts_with("../");
        let is_root_relative = specifier.starts_with('/');
        if !is_relative && !is_root_relative && !Path::new(specifier).is_absolute() {
            // Builtins bypass tsconfig paths entirely: Node guarantees node:
            // (and bare builtin names) never hit userland resolution, and
            // require() in the same project would disagree otherwise — two
            // identities for 'fs' in one run. oam: runtime modules get the
            // same guarantee.
            if specifier.starts_with("node:")
                || specifier.starts_with("oam:")
                || crate::npm::is_node_builtin(specifier)
            {
                return crate::npm::resolve_bare(specifier, referrer, crate::npm::ResolveMode::Import);
            }
            // Bare specifier: tsconfig paths get first crack (plan §2.6 — the
            // resolver honors tsconfig exactly as tsgo does), then the Node ESM
            // node_modules walk.
            let mut consulted_paths = false;
            if let Some(config) = crate::tsconfig::load_for_with(self, referrer) {
                consulted_paths = true;
                for raw in crate::tsconfig::match_specifier(&config, specifier) {
                    if let (Some(found), _) = crate::probe_candidates(self, &raw) {
                        return Ok(found);
                    }
                }
            }
            return crate::npm::resolve_bare(specifier, referrer, crate::npm::ResolveMode::Import)
                .map_err(|mut failure| {
                    if consulted_paths && failure.code == "OAM-MOD0002" {
                        failure
                            .message
                            .push_str(" (tsconfig paths were consulted; no pattern produced a file)");
                    }
                    failure
                });
        }

        let raw = if is_relative {
            let base = referrer.parent().unwrap_or_else(|| Path::new("."));
            base.join(specifier)
        } else if is_root_relative && !Path::new(specifier).is_absolute() {
            // Windows: '/x' is drive-relative — anchor it at the referrer's root
            // so behavior matches POSIX ('/' = filesystem root of the referrer).
            let mut root: PathBuf = referrer
                .components()
                .take_while(|c| {
                    matches!(
                        c,
                        std::path::Component::Prefix(_) | std::path::Component::RootDir
                    )
                })
                .collect();
            root.push(specifier.trim_start_matches('/'));
            root
        } else {
            PathBuf::from(specifier)
        };

        let (found, candidates) = crate::probe_candidates(self, &raw);
        found.ok_or_else(|| {
            Diagnostic::new(
                "OAM-MOD0001",
                Severity::Error,
                Origin::Resolve,
                format!(
                    "cannot resolve '{specifier}' from {} (tried {})",
                    referrer.display(),
                    candidates
                        .iter()
                        .map(|c| c.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })
    }
}

// ── Thread-local default Resolver ────────────────────────────────────────
//
// The free functions in `oam_loader` (the existing public API) need a
// `&Resolver`. We keep them working without forcing every caller to
// construct one by routing them through a per-thread default. Each thread
// gets its own default `Resolver` lazily, so a long-lived daemon with many
// worker threads won't see one thread's `clear_caches()` wipe another's
// work-in-progress -- each thread's default is isolated.
//
// The thread-local stores a `Box<Resolver>` that is leaked on first use,
// giving a `&'static Resolver` (the box lives for the thread's lifetime, the
// thread lives for the process's lifetime). That's the same shape the old
// process-globals had (also `OnceLock`-lived forever).

thread_local! {
    static DEFAULT: &'static Resolver = {
        let boxed: &'static Resolver = Box::leak(Box::new(Resolver::new()));
        boxed
    };
}

/// Borrow the current thread's default `Resolver`. Free functions call this
/// to preserve backward-compatible behavior. Long-lived consumers (CLI
/// tools, daemons) should construct their own `Resolver` and pass it
/// explicitly via the `*_with` variants.
pub fn default_resolver() -> &'static Resolver {
    DEFAULT.with(|r| *r)
}

/// Drop every cache entry on the current thread's default `Resolver`.
/// `oam install` calls this so a same-process `oam run` after `oam install`
/// sees the freshly-installed files instead of a stale "not found".
pub fn default_clear_caches() {
    default_resolver().clear_caches();
}
