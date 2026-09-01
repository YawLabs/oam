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

/// A loaded tsconfig entry: either a parsed `PathsConfig` (may be empty) or
/// the explicit `None` we cached for a tsconfig that had no `paths` field.
pub type TsconfigEntry = Option<crate::tsconfig::TsconfigInfo>;

/// Owns the per-resolver caches that used to be process-globals.
#[derive(Default)]
pub struct Resolver {
    /// Paths confirmed NOT to exist on a recent resolve. Speeds up repeated
    /// misses for the same specifier across the same resolver scope.
    negative_probes: Mutex<HashSet<PathBuf>>,
    /// `tsconfig.json` path -> parsed paths config. None = file existed but
    /// had no paths (so we don't re-stat it).
    tsconfigs: Mutex<HashMap<PathBuf, TsconfigEntry>>,
    /// Directory -> the nearest `tsconfig.json` at or above it (None =
    /// walked to the root and found nothing). Consulted before any stat, so
    /// a project with no tsconfig pays the ancestor walk once per
    /// directory, not once per resolve.
    tsconfig_dirs: Mutex<HashMap<PathBuf, Option<PathBuf>>>,
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
        self.tsconfig_dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub(crate) fn tsconfigs(&self) -> &Mutex<HashMap<PathBuf, TsconfigEntry>> {
        &self.tsconfigs
    }

    /// Nearest `tsconfig.json` at or above `referrer` (a file or a
    /// directory), through this resolver's discovery cache. Pure discovery:
    /// no parsing, no node_modules boundary (that applies when the OPTIONS
    /// are consumed). None when no ancestor has one.
    pub fn find_tsconfig(&self, referrer: &Path) -> Option<PathBuf> {
        crate::tsconfig::find_tsconfig_with(self, referrer)
    }

    /// The cached ancestor walk behind `find_tsconfig`: every directory
    /// visited on the way to an answer is recorded with that answer, so the
    /// next lookup from anywhere in the same subtree is a single map hit.
    pub(crate) fn nearest_tsconfig_from_dir(&self, start: PathBuf) -> Option<PathBuf> {
        let mut visited: Vec<PathBuf> = Vec::new();
        let mut dir = start;
        let found = loop {
            {
                let cache = self.tsconfig_dirs.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(hit) = cache.get(&dir) {
                    break hit.clone();
                }
            }
            let candidate = dir.join("tsconfig.json");
            visited.push(dir.clone());
            if candidate.is_file() {
                break Some(candidate);
            }
            if !dir.pop() {
                break None;
            }
        };
        let mut cache = self.tsconfig_dirs.lock().unwrap_or_else(|e| e.into_inner());
        for visited_dir in visited {
            cache.insert(visited_dir, found.clone());
        }
        found
    }

    /// Resolve a `require()` specifier from the CJS module at `referrer`,
    /// through this resolver's caches. See the free `resolve_require` for
    /// the algorithm.
    pub fn resolve_require(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
        crate::npm::resolve_require_with(self, specifier, referrer)
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
    /// an extension), TS-source fallback for JS extensions ('./x.js' -> x.ts
    /// then x.tsx, './x.jsx' -> x.tsx, './x.mjs' -> x.mts, './x.cjs' -> x.cts;
    /// the tsc rewrite convention), then extensionless probing (.ts, .tsx,
    /// .mts, .js, .jsx, .mjs) and directory index (index.ts, index.tsx,
    /// index.js) -- see `probe_candidates`.
    ///
    /// Bare specifiers resolve via tsconfig paths (if a tsconfig.json is found
    /// in the referrer's ancestor tree) then the Node ESM node_modules walk.
    ///
    /// `node:` / `oam:` specifiers resolve to virtual builtin paths.
    /// `node:`-prefixed builtins not yet in wave 1 surface OAM-MOD0006.
    ///
    /// Successful file resolutions are realpath'd (Node's default
    /// no-preserve-symlinks semantics): the module's identity is its real
    /// location, which is what makes pnpm symlink/junction layouts resolve
    /// transitive deps correctly.
    pub fn resolve_import(&self, specifier: &str, referrer: &Path) -> Result<PathBuf, Diagnostic> {
        self.resolve_import_inner(specifier, referrer)
            .map(crate::pathutil::finalize_resolved)
    }

    fn resolve_import_inner(
        &self,
        specifier: &str,
        referrer: &Path,
    ) -> Result<PathBuf, Diagnostic> {
        // Shapes that are invalid as ESM specifiers everywhere — npm resolution
        // will never fix these, so they get their own diagnostic, not MOD0002.
        if specifier.is_empty() || specifier == "." || specifier == ".." || specifier.contains('\\')
        {
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

        // file: URLs are valid ESM specifiers (static and dynamic import):
        // convert to a filesystem path and probe like any absolute import.
        // Other URL schemes (data:, https:) are unsupported and fall through
        // to the bare-specifier diagnostics below.
        if specifier
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("file:"))
        {
            let raw = crate::pathutil::file_url_to_path(specifier).map_err(|why| {
                Diagnostic::new(
                    "OAM-MOD0002",
                    Severity::Error,
                    Origin::Resolve,
                    format!(
                        "cannot resolve '{specifier}' in {}: {why}",
                        referrer.display()
                    ),
                )
            })?;
            let (found, candidates) = crate::probe_candidates(self, &raw);
            return found.ok_or_else(|| {
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
            });
        }

        let is_relative = specifier.starts_with("./") || specifier.starts_with("../");
        let is_root_relative = specifier.starts_with('/');
        if !is_relative && !is_root_relative && !Path::new(specifier).is_absolute() {
            // Builtins bypass tsconfig paths entirely: Node guarantees node:
            // (and bare builtin names) never hit userland resolution, and
            // require() in the same project would disagree otherwise — two
            // identities for 'fs' in one run. oam: runtime modules get the
            // same guarantee. (`resolve_require_inner` hoists the identical
            // guard above its paths block.)
            if crate::is_builtin_specifier(specifier) {
                return crate::npm::resolve_bare(
                    specifier,
                    referrer,
                    crate::npm::ResolveMode::Import,
                );
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
                        failure.message.push_str(
                            " (tsconfig paths were consulted; no pattern produced a file)",
                        );
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
///
/// **Scope note**: this is thread-local. The default `Resolver` is one per
/// thread, so this clear affects ONLY the calling thread. A worker thread
/// that resolves AFTER another thread's install has its own `Resolver`
/// and its own (possibly stale) cache. For a multi-threaded embedder
/// (server worker pool, daemon), construct one shared `Resolver` and call
/// `clear_caches()` on it after layout-mutating operations -- do not rely
/// on the thread-local default for cross-thread cache invalidation. For
/// the single-thread CLI use case (`oam install` exits the process, then
/// a fresh `oam run` is a new process) this scope is sufficient.
pub fn default_clear_caches() {
    default_resolver().clear_caches();
}
