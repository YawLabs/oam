//! Lockfile-driven package installer (MVP: frozen-lockfile mode only).
//!
//! Reads npm's `package-lock.json` v3, downloads tarballs from the resolved
//! URLs, verifies SRI integrity hashes, extracts into `node_modules`, and
//! creates `.bin` shims. The `npm ci` counterpart for oam projects: it makes
//! `node_modules` match the lockfile -- a package whose installed version
//! differs from the lockfile's is re-extracted, a package the lockfile no
//! longer names is removed, and an optional entry for another platform
//! (`os` / `cpu`) is skipped.
//!
//! Not `npm ci` yet: workspace `link` entries are not created (skipped with
//! a warning), `libc` is not filtered on, nested packages get no
//! `node_modules/.bin` of their own, and stale `.bin` shims are not removed.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oam_diagnostics::{Diagnostic, Origin, Severity};
use serde::Deserialize;

// ── Lockfile structs ────────────────────────────────────────────────────

/// Top-level `package-lock.json` (v3 only).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lockfile {
    lockfile_version: u32,
    #[serde(default)]
    packages: HashMap<String, LockfileEntry>,
}

/// A single entry in the `packages` map.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct LockfileEntry {
    /// The package's real name. npm writes it only when it differs from the
    /// directory name in the key (an alias: `"x": "npm:other@1"`).
    name: Option<String>,
    version: Option<String>,
    resolved: Option<String>,
    integrity: Option<String>,
    /// The `bin` field inside the lockfile entry (npm v3 mirrors it from
    /// the package's own package.json).
    bin: Option<serde_json::Value>,
    /// A workspace / `file:` directory link: `resolved` is then a path
    /// relative to the project, not a URL.
    #[serde(default)]
    link: bool,
    /// Reachable only through optional dependencies: skipped on a platform
    /// it does not support, and its failure does not fail the install.
    #[serde(default)]
    optional: bool,
    /// `process.platform` values the package supports (`!x` excludes one).
    os: Option<serde_json::Value>,
    /// `process.arch` values the package supports (`!x` excludes one).
    cpu: Option<serde_json::Value>,
}

/// This build's `process.platform`.
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        "solaris" | "illumos" => "sunos",
        other => other,
    }
}

/// This build's `process.arch`.
fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "ia32",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        other => other,
    }
}

/// npm's `os` / `cpu` list check (npm-install-checks `checkList`): `value`
/// must match none of the `!negated` entries and, when the list has plain
/// entries, at least one of them. A missing list, an empty one and `["any"]`
/// allow everything.
fn platform_list_allows(list: Option<&serde_json::Value>, value: &str) -> bool {
    use serde_json::Value;
    let entries: Vec<&str> = match list {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(|v| v.as_str()).collect(),
        _ => return true,
    };
    if entries.len() == 1 && entries[0] == "any" {
        return true;
    }
    let mut negated = 0usize;
    let mut matched = false;
    for entry in &entries {
        if let Some(excluded) = entry.strip_prefix('!') {
            negated += 1;
            if excluded == value {
                return false;
            }
        } else if *entry == value {
            matched = true;
        }
    }
    matched || negated == entries.len()
}

/// Whether a lockfile entry's `os` and `cpu` lists admit this build.
fn entry_matches_platform(entry: &LockfileEntry) -> bool {
    platform_list_allows(entry.os.as_ref(), node_platform())
        && platform_list_allows(entry.cpu.as_ref(), node_arch())
}

/// The package directory name a lockfile key installs under, or `None` when
/// the key is not one `oam install` may write to.
///
/// A key is a `/`-separated path relative to the project that ends in
/// `node_modules/<name>` or `node_modules/@scope/<name>` (optionally nested,
/// optionally under a workspace directory). Every segment must be an ordinary
/// name: no empty, `.` or `..` segment, no backslash, drive colon or NUL --
/// so `project_dir.join(key)` cannot leave the project -- and the package
/// name itself may not start with a dot (`.bin`, `.oam`).
fn lockfile_key_package(key: &str) -> Option<&str> {
    if key.contains(['\\', ':', '\0']) {
        return None;
    }
    let segments: Vec<&str> = key.split('/').collect();
    if segments.iter().any(|s| matches!(*s, "" | "." | "..")) {
        return None;
    }
    let anchor = segments.iter().rposition(|s| *s == "node_modules")?;
    let name_len = match &segments[anchor + 1..] {
        [name] if !name.starts_with(['.', '@']) => name.len(),
        [scope, name] if scope.starts_with('@') && scope.len() > 1 && !name.starts_with('.') => {
            scope.len() + 1 + name.len()
        }
        _ => return None,
    };
    Some(&key[key.len() - name_len..])
}

/// Whether the package installed at `manifest_path` is a different version
/// from the one the lockfile names. Anything unknown -- no lockfile version,
/// an unreadable manifest, a manifest without a version -- counts as "same":
/// the package is left alone rather than re-downloaded on a guess.
fn installed_version_differs(manifest_path: &Path, want: Option<&str>) -> bool {
    let Some(want) = want else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(manifest_path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    // The lockfile holds the cleaned version; a manifest may still say
    // "v1.2.3" or "=1.2.3".
    let clean = |v: &str| v.trim().trim_start_matches(['v', '=']).to_string();
    match json.get("version").and_then(|v| v.as_str()) {
        Some(have) => clean(have) != clean(want),
        None => false,
    }
}

/// Summary returned on a clean install (every package installed).
#[derive(Debug)]
pub struct InstallSummary {
    pub packages_installed: usize,
    pub elapsed: Duration,
}

/// The two outcomes `install` can return through the success channel; the
/// "didn't even start" set lands in `Err(Vec<Diagnostic>)`.
///
/// `Err(Vec<Diagnostic>)` is reserved for "didn't even start" — the lockfile
/// was missing or unparseable, the lockfileVersion was unsupported, the
/// async runtime / HTTP client could not be initialized, or the install was
/// invoked in non-frozen mode. No packages were attempted in any of those
/// cases, so there is nothing for the caller to recover from.
///
/// `Ok(Partial { .. })` means at least one install pass was attempted: some
/// packages installed (counted in `installed`, including cached ones) and
/// `errors` is non-empty. The caller decides whether to treat a Partial as
/// success (e.g. `oam install` exiting 0 with bin-shim warnings) or failure
/// (default for CI).
///
/// `Ok(Complete(_))` means every package installed cleanly; `errors` is
/// empty in this arm.
///
/// Diagnostic-code mapping (see also the doc on `install`):
///
/// * `Ok(Complete(_))` — no diagnostics.
/// * `Ok(Partial { errors, .. })` — `OAM-PKG0004` (per-package fetch /
///   extract failure) and `OAM-PKG0005` (bin shim warning). The `installed`
///   count and `errors` list together describe what landed.
/// * `Err(_)` — `OAM-PKG0001` (lockfile read or parse failure),
///   `OAM-PKG0002` (unsupported lockfileVersion), `OAM-PKG0003` (async
///   runtime or HTTP client init failure), `OAM-PKG0006` (non-frozen mode
///   rejected). No packages were attempted.
#[derive(Debug)]
pub enum InstallOutcome {
    /// Every package installed cleanly.
    Complete(InstallSummary),
    /// Some packages installed, some failed (and/or bin-shim warnings).
    /// `installed` is the count of successful installs (incl. cached).
    /// `errors` is non-empty; the caller decides whether to treat as success.
    Partial {
        installed: usize,
        elapsed: Duration,
        errors: Vec<Diagnostic>,
    },
}

// ── Public entry point ──────────────────────────────────────────────────

/// Install packages from a lockfile.
///
/// `project_dir` is the directory containing `package-lock.json`.
/// `frozen` must be `true` for the MVP (the lockfile is authoritative; no
/// resolution or lockfile mutation).
///
/// **Concurrency**: this function takes a best-effort advisory lock on
/// `node_modules/.oam/install.lock` (std `File::lock`), so two concurrent
/// `oam install` runs on the same project serialize instead of racing
/// `link_bins` / the package-presence idempotency check. The lock is held for
/// the duration of the install and released when the function returns (RAII).
/// If the lock cannot be set up (unusual filesystem), the install proceeds
/// unlocked rather than failing -- and individual package extracts are atomic
/// (unique tempdir + rename) regardless, so the worst unlocked race is a
/// harmless last-writer-wins on bin shims.
///
/// **Manifest cache**: package.json files are read through a process-global
/// cache (`cached_manifest`) that survives `Resolver::clear_caches()`. The
/// next `oam install` in the same process will re-read each manifest
/// because `read_lockfile` doesn't touch that cache, but a long-lived
/// embedder that mutates a package.json after its first resolve will see
/// the stale parsed value. Restart the process to drop the cache.
///
/// Returns one of:
///
/// * `Ok(InstallOutcome::Complete(summary))` — every package installed; the
///   `summary` carries the count and elapsed time.
/// * `Ok(InstallOutcome::Partial { installed, elapsed, errors })` — some
///   packages installed (count includes cached) and some failed, OR a
///   non-fatal bin-shim warning surfaced. The caller decides whether to
///   treat this as success or failure.
/// * `Err(Vec<Diagnostic>)` — the install didn't even start. Covers:
///   lockfile missing, lockfile parse failure, unsupported
///   `lockfileVersion`, async runtime or HTTP client init failure, or
///   non-frozen mode rejected. No packages were attempted.
///
/// Diagnostic-code mapping (this is load-bearing — the outcome variant
/// alone does not tell the caller which subsystem failed):
///
/// * `Ok(Complete(_))` — no diagnostics.
/// * `Ok(Partial { errors, .. })` — `OAM-PKG0004` (per-package fetch /
///   extract failure; as a `Severity::Warning` it is a package-level note
///   that does not fail the install: an optional package that could not be
///   installed, a download with no integrity hash to check, tarball symlinks
///   that were not created, workspace links oam does not create) and
///   `OAM-PKG0005` (bin shim warning).
/// * `Err(_)` — `OAM-PKG0001` (lockfile read or parse),
///   `OAM-PKG0002` (unsupported version), `OAM-PKG0003` (runtime or HTTP
///   client init), `OAM-PKG0006` (non-frozen mode rejected).
pub fn install(
    project_dir: &Path,
    frozen: bool,
    precompile: bool,
) -> Result<InstallOutcome, Vec<Diagnostic>> {
    if !frozen {
        return Err(vec![diag(
            "OAM-PKG0006",
            "only --frozen-lockfile mode is supported in this release",
        )]);
    }
    // Install mutates the project layout (it writes node_modules/, and a
    // package it extracts can carry a tsconfig that a project `extends`). A
    // same-process `oam run` after `oam install` must not see the stale
    // "not found" / stale parsed paths. Drop the current thread's default
    // resolver caches so the next resolve re-stats the filesystem and
    // re-reads tsconfig.
    crate::resolver::default_clear_caches();
    let started = Instant::now();

    let lockfile_path = project_dir.join("package-lock.json");
    let lockfile = read_lockfile(&lockfile_path)?;

    if lockfile.lockfile_version != 3 {
        return Err(vec![diag(
            "OAM-PKG0002",
            format!(
                "unsupported lockfileVersion {} (expected 3); regenerate with npm 7+",
                lockfile.lockfile_version
            ),
        )]);
    }

    let node_modules = project_dir.join("node_modules");

    // A5: serialize concurrent `oam install` runs on the same project with an
    // advisory lock under node_modules/.oam. Best-effort -- proceed unlocked if
    // it can't be set up. Held for the rest of the function; dropping it
    // releases the lock (RAII), so every early return below also unlocks.
    let _install_lock = acquire_install_lock(&node_modules);

    // Collect non-root packages (the "" key is the project root).
    let mut to_install: Vec<(&str, &LockfileEntry)> = lockfile
        .packages
        .iter()
        .filter(|(key, _)| !key.is_empty())
        .map(|(k, v)| (k.as_str(), v))
        .collect();
    // Sort for deterministic install order.
    to_install.sort_by_key(|(k, _)| *k);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            vec![diag(
                "OAM-PKG0003",
                format!("failed to create async runtime: {e}"),
            )]
        })?;

    let client = build_http_client().map_err(|e| {
        vec![diag(
            "OAM-PKG0003",
            format!("failed to create HTTP client: {e}"),
        )]
    })?;

    let mut installed = 0usize;
    let mut errors: Vec<Diagnostic> = Vec::new();
    // A4: lifecycle scripts for TRUSTED packages, collected during the extract
    // loop and run in one pass AFTER link_bins, so a script can call tools in
    // node_modules/.bin (which isn't populated until then).
    let mut pending_scripts: Vec<PendingScripts> = Vec::new();
    let trust = crate::trust::TrustConfig::load(project_dir);
    // Per-package "scripts already ran" stamps live here. A marker is written
    // AFTER a package's lifecycle scripts complete successfully, so a warm
    // install can tell "extracted AND scripts ran" (skip) apart from
    // "extracted but scripts never ran" (a crash between extract and the
    // script pass -- run them once now). This is what gives oam npm's
    // run-once semantics across repeated install() calls.
    let scripts_ran_dir = node_modules.join(".oam").join("scripts-ran");
    let cache_root = project_dir
        .join("node_modules")
        .join(".oam")
        .join("precompile");

    // Plan: the lockfile entries this run installs, as (key, directory name,
    // entry). Everything else is decided here, once, so the install loop, the
    // bin pass and the precompile pass all walk the same list.
    let mut plan: Vec<(&str, &str, &LockfileEntry)> = Vec::new();
    let mut unlinked: Vec<&str> = Vec::new();
    for &(key, entry) in &to_install {
        if entry.link {
            // A workspace / `file:` directory link: `resolved` is a project
            // path, never something to download. oam does not create the
            // link; say so unless something (npm) already put it there.
            if lockfile_key_package(key).is_some()
                && std::fs::symlink_metadata(project_dir.join(key)).is_err()
            {
                unlinked.push(key);
            }
            continue;
        }
        if entry.resolved.is_none() {
            // No tarball of its own: the root, a workspace's source
            // directory, or a dependency bundled inside its parent's tarball.
            continue;
        }
        // The key is e.g. "node_modules/foo", "node_modules/@scope/bar" or,
        // nested, "node_modules/react/node_modules/scheduler"; the package's
        // directory name is what follows the LAST "node_modules/". A key of
        // any other shape is never joined to project_dir.
        let Some(pkg_name) = lockfile_key_package(key) else {
            errors.push(diag(
                "OAM-PKG0004",
                format!(
                    "failed to install {key}: the lockfile key is not a node_modules/<name> path inside the project"
                ),
            ));
            continue;
        };
        if !entry_matches_platform(entry) {
            // npm skips an optional package built for another platform and
            // refuses a required one (EBADPLATFORM).
            if !entry.optional {
                errors.push(diag(
                    "OAM-PKG0004",
                    format!(
                        "failed to install {key}: the package does not support this platform ({} {}) and is not optional",
                        node_platform(),
                        node_arch()
                    ),
                ));
            }
            continue;
        }
        plan.push((key, pkg_name, entry));
    }
    if !unlinked.is_empty() {
        errors.push(Diagnostic::new(
            "OAM-PKG0004",
            Severity::Warning,
            Origin::Install,
            format!(
                "{} workspace/link package(s) not linked into node_modules (oam install does not create links yet; `npm install` does): {}",
                unlinked.len(),
                unlinked.join(", ")
            ),
        ));
    }

    for &(key, pkg_name, entry) in &plan {
        let Some(resolved) = entry.resolved.as_deref() else {
            continue;
        };
        // The extraction target is project_dir joined with the (validated) key.
        let dest = project_dir.join(key);
        // The name lifecycle-script trust is checked against: the lockfile's
        // `name` when the package is installed under an alias, else the
        // directory name.
        let declared_name = entry.name.as_deref().unwrap_or(pkg_name);

        // Skip already-installed packages (idempotency for partial installs).
        // WARM path: the package was extracted by a prior install run. npm runs
        // pre/install/postinstall exactly ONCE, at extract time -- not on every
        // resolve -- so we do NOT re-run scripts here just because the package
        // is present. We DO run them if the stamp marker is absent, which means
        // a prior run extracted the package but died before the script pass
        // (so the scripts genuinely never ran). The marker is written only
        // after a successful run, below.
        //
        // "Installed" means the version the lockfile names: a package left
        // over from before a lockfile bump is STALE and is re-extracted.
        let manifest_path = dest.join("package.json");
        let present = manifest_path.is_file();
        let stale = present && installed_version_differs(&manifest_path, entry.version.as_deref());
        if present && !stale {
            installed += 1;
            if !script_marker_path(&scripts_ran_dir, pkg_name).is_file()
                && let Some(ps) = check_lifecycle_scripts(
                    &dest,
                    pkg_name,
                    declared_name,
                    Some(resolved),
                    &scripts_ran_dir,
                    &trust,
                    &mut errors,
                )
            {
                pending_scripts.push(ps);
            }
            continue;
        }

        // Only http(s) tarballs are fetched. Anything else (`file:`, `git+`,
        // a bare path) is refused here rather than handed to the HTTP client
        // and retried.
        if !(resolved.starts_with("https://") || resolved.starts_with("http://")) {
            errors.push(diag(
                "OAM-PKG0004",
                format!(
                    "failed to install {key}: unsupported `resolved` value '{resolved}' (only http(s) tarball URLs are installed)"
                ),
            ));
            continue;
        }

        let mut notes: Vec<String> = Vec::new();
        match fetch_and_extract(
            &rt,
            &client,
            resolved,
            entry.integrity.as_deref(),
            &dest,
            stale,
            &mut notes,
        ) {
            Ok(()) => {
                installed += 1;
                if entry.integrity.is_none() {
                    errors.push(Diagnostic::new(
                        "OAM-PKG0004",
                        Severity::Warning,
                        Origin::Install,
                        format!(
                            "{key}: the lockfile entry has no integrity hash, so the download from {resolved} was not verified"
                        ),
                    ));
                }
                if !notes.is_empty() {
                    errors.push(Diagnostic::new(
                        "OAM-PKG0004",
                        Severity::Warning,
                        Origin::Install,
                        format!(
                            "{key}: {} symlink(s) in the tarball were not created: {}",
                            notes.len(),
                            summarize_notes(&notes)
                        ),
                    ));
                }
                if stale {
                    // A new version replaced the old one: its scripts have
                    // not run, whatever the old version's stamp says.
                    let _ = std::fs::remove_file(script_marker_path(&scripts_ran_dir, pkg_name));
                }
                // FRESH path: the package was just extracted this call, so its
                // scripts have never run -- enqueue them unconditionally.
                if let Some(ps) = check_lifecycle_scripts(
                    &dest,
                    pkg_name,
                    declared_name,
                    Some(resolved),
                    &scripts_ran_dir,
                    &trust,
                    &mut errors,
                ) {
                    pending_scripts.push(ps);
                }
            }
            Err(e) if entry.optional => {
                // npm carries on without an optional dependency that fails.
                errors.push(Diagnostic::new(
                    "OAM-PKG0004",
                    Severity::Warning,
                    Origin::Install,
                    format!("optional package {key} was not installed: {e}"),
                ));
            }
            Err(e) => {
                errors.push(diag("OAM-PKG0004", format!("failed to install {key}: {e}")));
            }
        }
    }

    // Prune: a package directory the lockfile does not name is left over
    // from an earlier dependency set, and resolution would still find it.
    let known_keys: HashSet<&str> = to_install.iter().map(|(key, _)| *key).collect();
    prune_unlisted(&node_modules, "node_modules", &known_keys);

    // Bin linking pass (runs even if some packages failed — link what we can).
    // Bin failures are non-fatal: a missing .bin shim doesn't invalidate the
    // install of the package itself, so the diagnostic lands on the Partial
    // path instead of poisoning an otherwise-clean install with Err.
    let linkable: Vec<(&str, &LockfileEntry)> =
        plan.iter().map(|&(key, _, entry)| (key, entry)).collect();
    link_bins(&node_modules, &linkable, &mut errors);

    // A4: now that the tree and .bin shims are in place, run the lifecycle
    // scripts collected from trusted packages.
    run_lifecycle_scripts(&pending_scripts, &node_modules, &mut errors);

    // --precompile walks EVERY package in the lockfile, not just the ones
    // extracted by THIS call: a warm tree (a prior `oam install` without the
    // flag, or an npm-installed node_modules) is populated and refreshed too.
    // It runs after the lifecycle-script pass so the tree is final, and the
    // per-file freshness header (transpile fingerprint + source hash) keeps
    // the warm cost to one read + one hash + one open per source file.
    if precompile {
        // `plan`, not the raw lockfile: link entries, entries for another
        // platform and keys that are not package paths are out, as in the
        // install loop above.
        for &(key, _, _) in &plan {
            let dest = project_dir.join(key);
            if !dest.join("package.json").is_file() {
                // Failed to install this run -- already diagnosed above.
                continue;
            }
            // Cache key = the lockfile key relative to the project's
            // node_modules ("react/node_modules/scheduler" for a nested
            // package), so nested packages get distinct cache slots and the
            // run-time reader -- anchored on the innermost node_modules
            // ancestor holding a `.oam/precompile` directory, i.e. this
            // project's -- derives the same path.
            let cache_key = key.strip_prefix("node_modules/").unwrap_or(key);
            let (_, precompile_errors) =
                crate::precompile::precompile_package(&dest, cache_key, &cache_root);
            push_precompile_warning(cache_key, &precompile_errors, &mut errors);
        }
    }

    let elapsed = started.elapsed();

    // Two-pass decision: gather every error first, then classify. That way
    // the empty-errors branch is a single line and the partial branch carries
    // everything we accumulated without re-walking.
    if errors.is_empty() {
        Ok(InstallOutcome::Complete(InstallSummary {
            packages_installed: installed,
            elapsed,
        }))
    } else {
        Ok(InstallOutcome::Partial {
            installed,
            elapsed,
            errors,
        })
    }
}

/// Fold a package's per-file precompile failures into ONE `OAM-PKG0008`
/// warning naming the count and the first few failing files. Precompile is
/// an optimization -- an uncompiled file falls back to runtime transpile --
/// so failures never fail the install, and one bad file does not hide how
/// many others failed with it.
fn push_precompile_warning(
    pkg_key: &str,
    failures: &[crate::precompile::PrecompileError],
    errors: &mut Vec<Diagnostic>,
) {
    if failures.is_empty() {
        return;
    }
    const SHOWN: usize = 3;
    let mut detail = failures
        .iter()
        .take(SHOWN)
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    if failures.len() > SHOWN {
        detail.push_str(&format!(" (+{} more)", failures.len() - SHOWN));
    }
    errors.push(Diagnostic::new(
        "OAM-PKG0008",
        Severity::Warning,
        Origin::Install,
        format!(
            "precompile {pkg_key}: {} file(s) failed: {detail}",
            failures.len()
        ),
    ));
}

/// The first few per-entry notes of one package, joined for a single
/// diagnostic line.
fn summarize_notes(notes: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut detail = notes
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    if notes.len() > SHOWN {
        detail.push_str(&format!(" (+{} more)", notes.len() - SHOWN));
    }
    detail
}

/// Remove the packages in `dir` (a node_modules directory whose lockfile key
/// is `dir_key`) that the lockfile does not name, then do the same inside
/// every package it does name.
///
/// Only package-shaped entries go: a directory holding a `package.json`, or a
/// link. Dot-entries (`.bin`, `.oam`, tool caches) and anything else that is
/// not a package are left alone. Best-effort: a removal that fails is left
/// for the next install.
fn prune_unlisted(dir: &Path, dir_key: &str, known: &HashSet<&str>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for item in read.flatten() {
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let path = item.path();
        if !name.starts_with('@') {
            prune_candidate(&path, &format!("{dir_key}/{name}"), known);
            continue;
        }
        // A scope directory: its children are the packages. A scope that is
        // itself a link is not walked.
        let is_real_dir = std::fs::symlink_metadata(&path)
            .map(|m| m.is_dir() && !m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_real_dir {
            continue;
        }
        let Ok(scoped) = std::fs::read_dir(&path) else {
            continue;
        };
        for sub in scoped.flatten() {
            let Ok(sub_name) = sub.file_name().into_string() else {
                continue;
            };
            if sub_name.starts_with('.') {
                continue;
            }
            let sub_key = format!("{dir_key}/{name}/{sub_name}");
            prune_candidate(&sub.path(), &sub_key, known);
        }
    }
}

/// One entry of a node_modules directory for [`prune_unlisted`]: keep and
/// descend when the lockfile names `key`, remove when it is an unlisted
/// package. A link is removed as a link -- its target is never touched.
fn prune_candidate(path: &Path, key: &str, known: &HashSet<&str>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let is_link = meta.file_type().is_symlink();
    if known.contains(key) {
        if !is_link && meta.is_dir() {
            let nested_key = format!("{key}/node_modules");
            prune_unlisted(&path.join("node_modules"), &nested_key, known);
        }
        return;
    }
    if is_link {
        let _ = std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path));
    } else if meta.is_dir() && path.join("package.json").is_file() {
        let _ = std::fs::remove_dir_all(path);
    }
}

/// Acquire the cross-process install lock (A5). Returns the held lock file
/// (drop releases it) or `None` if the lock could not be set up -- in which
/// case the caller proceeds unlocked. `File::lock` blocks until any concurrent
/// holder releases, which is exactly the desired serialize behavior; only a
/// setup *error* (dir/file creation, lock syscall failure) yields `None`.
fn acquire_install_lock(node_modules: &Path) -> Option<std::fs::File> {
    let oam_dir = node_modules.join(".oam");
    std::fs::create_dir_all(&oam_dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(oam_dir.join("install.lock"))
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

// ── Lockfile I/O ────────────────────────────────────────────────────────

fn read_lockfile(path: &Path) -> Result<Lockfile, Vec<Diagnostic>> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        vec![diag(
            "OAM-PKG0001",
            format!(
                "cannot read {}: {e} (run `npm install` to generate a lockfile)",
                path.display()
            ),
        )]
    })?;
    let lockfile: Lockfile = serde_json::from_str(&raw).map_err(|e| {
        vec![diag(
            "OAM-PKG0001",
            format!("failed to parse {}: {e}", path.display()),
        )]
    })?;
    Ok(lockfile)
}

// ── HTTP client ─────────────────────────────────────────────────────────

/// The HTTPS client `oam install` fetches with, and `oam self-update` too
/// (crates/oam_cli/src/self_update.rs): one TLS setup, one user agent.
pub fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    http_client_builder().build()
}

/// The installer's client, before `build()` (a test adds `no_proxy()`, so a
/// proxy in the environment cannot stand between it and its loopback server).
fn http_client_builder() -> reqwest::ClientBuilder {
    // reqwest with rustls-no-provider needs an explicit provider install.
    // The ring provider is already compiled (workspace dep via rustls).
    let _ = rustls::crypto::ring::default_provider().install_default();

    reqwest::ClientBuilder::new()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("oam/", env!("CARGO_PKG_VERSION")))
}

// ── Fetch + extract ─────────────────────────────────────────────────────

fn fetch_and_extract(
    rt: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    url: &str,
    integrity: Option<&str>,
    dest: &Path,
    replace: bool,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    // Retry up to 3 times: registry CDNs see transient 5xx and TLS resets on
    // CI runners; npm itself defaults to 5. One retry (the original) gave up
    // too easily for flaky-network environments without adding meaningful
    // resilience over zero retries.
    let data = download_with_retry(rt, client, url, 3)?;

    // An entry without an integrity hash is extracted unverified; the caller
    // reports that as a warning.
    if let Some(sri) = integrity {
        verify_integrity(&data, sri)?;
    }

    extract_tarball_with(&data, dest, replace, notes)
}

fn download_with_retry(
    rt: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    url: &str,
    retries: usize,
) -> Result<Vec<u8>, String> {
    let mut last_err = String::new();
    for attempt in 0..=retries {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500));
        }
        match rt.block_on(async {
            let resp = match client.get(url).send().await {
                Ok(resp) => resp,
                Err(e) => return Err(format!("request to {url}: {e}")),
            };
            let status = resp.status();
            if !status.is_success() {
                return Err(format!("HTTP {status} for {url}"));
            }
            match resp.bytes().await {
                Ok(bytes) => Ok(bytes.to_vec()),
                Err(e) => Err(format!("reading body from {url}: {e}")),
            }
        }) {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                last_err = e;
            }
        }
    }
    Err(last_err)
}

// ── SRI integrity verification ──────────────────────────────────────────

/// Check `data` against an SRI string: one or more whitespace-separated
/// `<algo>-<base64>[?options]` hashes. As npm (ssri) does, only the strongest
/// algorithm present is checked, and any one hash of that algorithm matching
/// passes; hashes of an algorithm oam does not know are ignored when a known
/// one is present.
pub(crate) fn verify_integrity(data: &[u8], integrity: &str) -> Result<(), String> {
    // Strongest first.
    const KNOWN: [&str; 4] = ["sha512", "sha384", "sha256", "sha1"];

    let mut hashes: Vec<(&str, &str)> = Vec::new();
    for token in integrity.split_whitespace() {
        let (algo, rest) = token
            .split_once('-')
            .ok_or_else(|| format!("invalid SRI format: {integrity}"))?;
        let hash_b64 = rest.split('?').next().unwrap_or(rest);
        hashes.push((algo, hash_b64));
    }
    let Some((first_algo, _)) = hashes.first() else {
        return Err(format!("invalid SRI format: {integrity}"));
    };
    let Some(algo) = KNOWN
        .iter()
        .find(|known| hashes.iter().any(|(a, _)| a == *known))
    else {
        return Err(format!("unsupported hash algorithm: {first_algo}"));
    };

    let computed: Vec<u8> = match *algo {
        "sha512" => {
            use sha2::{Digest, Sha512};
            Sha512::digest(data).to_vec()
        }
        "sha384" => {
            use sha2::{Digest, Sha384};
            Sha384::digest(data).to_vec()
        }
        "sha256" => {
            use sha2::{Digest, Sha256};
            Sha256::digest(data).to_vec()
        }
        _ => {
            use sha1::Digest;
            sha1::Sha1::digest(data).to_vec()
        }
    };

    let mut undecodable: Option<String> = None;
    for (_, hash_b64) in hashes.iter().filter(|(a, _)| a == algo) {
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, hash_b64) {
            Ok(expected) if expected == computed => return Ok(()),
            Ok(_) => {}
            Err(e) => undecodable = Some(format!("invalid base64 in integrity: {e}")),
        }
    }
    Err(undecodable.unwrap_or_else(|| format!("{algo} integrity mismatch")))
}

// ── Tarball extraction ──────────────────────────────────────────────────

#[cfg(test)]
fn extract_tarball(data: &[u8], dest: &Path) -> Result<(), String> {
    extract_tarball_with(data, dest, false, &mut Vec::new())
}

/// Extract a package tarball to `dest`.
///
/// `replace` is for a `dest` that holds a STALE install (another version):
/// the complete package already there is then replaced instead of being
/// taken for a peer's finished copy. `notes` collects one line per tarball
/// entry that was deliberately not created (see `create_symlink_entry`).
fn extract_tarball_with(
    data: &[u8],
    dest: &Path,
    replace: bool,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    // Atomic extract: extract into a sibling tempdir first, then rename to
    // `dest`. If anything fails mid-extract (ENOSPC, tar truncation, path
    // traversal, etc.) we remove the tempdir and `dest` stays untouched, so
    // the next install run will retry from scratch instead of seeing a
    // half-written package whose package.json exists (the idempotency
    // shortcut in install() trusts package.json presence).
    let parent = dest
        .parent()
        .ok_or_else(|| format!("extract destination has no parent dir: {}", dest.display()))?;
    let base = dest
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("extract destination has no file name: {}", dest.display()))?;
    // Per-process unique tempdir suffix. We can't randomize across processes
    // without a syscall, but combining pid + a monotonic counter is enough to
    // avoid collisions WITHIN this process. A racing sibling process picks a
    // different pid, so its tempdir is distinct too.
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp_id = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.oam-tmp-{}-{}",
        base,
        std::process::id(),
        tmp_id
    ));
    // A leftover tempdir from a prior crashed run with the same pid+counter
    // would be unusual but possible; clean it first so create_dir_all below
    // works on a fresh tree.
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .map_err(|e| format!("create extract tempdir {}: {e}", tmp.display()))?;

    let result = extract_into(data, &tmp, notes);
    match result {
        Ok(()) => {
            // A peer that FULLY installed dest (package.json present) between
            // the caller's idempotency check and now wins; drop our tempdir
            // and accept the peer's copy -- both are SRI-verified upstream,
            // so functionally equivalent. We key on package.json presence,
            // NOT bare existence: dest may exist as a stale/partial dir from
            // a prior crashed extract (dir created, package.json never
            // written), which must be replaced, not accepted. Under `replace`
            // the complete package at dest is the stale version this extract
            // is here to replace, so it is never accepted either.
            if !replace && dest.join("package.json").is_file() {
                let _ = std::fs::remove_dir_all(&tmp);
                return Ok(());
            }
            // Clear any stale/partial dest: rename() will not overwrite a
            // non-empty directory, so a leftover from a crashed run would
            // make the rename fail.
            if dest.exists() {
                let _ = std::fs::remove_dir_all(dest);
            }
            match std::fs::rename(&tmp, dest) {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Lost a rename race (a peer populated dest after our
                    // clear) or a transient FS error. If dest now has a
                    // package.json, accept the peer's copy; else surface it.
                    let _ = std::fs::remove_dir_all(&tmp);
                    if !replace && dest.join("package.json").is_file() {
                        Ok(())
                    } else {
                        Err(format!(
                            "rename {} -> {}: {e}",
                            tmp.display(),
                            dest.display()
                        ))
                    }
                }
            }
        }
        Err(e) => {
            // Pre-extract failure: dest still pristine. Tempdir cleanup is
            // best-effort -- if it fails the tempdir name is unique enough
            // that it won't poison a later install.
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

/// Whether any existing component of `rel`, walked from `dest`, is a symlink.
/// A write to `dest/rel` would then land wherever the link points, which the
/// lexical checks on `rel` say nothing about.
fn crosses_symlink(dest: &Path, rel: &Path) -> bool {
    let mut cur = dest.to_path_buf();
    for comp in rel.components() {
        cur.push(comp);
        let is_link = std::fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_link {
            return true;
        }
    }
    false
}

fn extract_into(data: &[u8], dest: &Path, notes: &mut Vec<String>) -> Result<(), String> {
    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);

    // Clean the dest once: `Path::join` followed by lexical `starts_with`
    // would falsely reject a non-canonical `dest` (e.g. `/tmp/./x` joined
    // with a normal file). Cleaning both sides puts the comparison on a
    // canonical basis.
    let dest_clean = path_clean(dest);
    // `dest` starts empty, so the only symlinks under it are the ones this
    // tarball's own entries created; until one exists there is nothing for
    // a later entry to be written through.
    let mut symlinks_created = false;

    for entry in archive.entries().map_err(|e| format!("tar entries: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let path = entry.path().map_err(|e| format!("tar entry path: {e}"))?;

        // An entry path is a chain of ordinary names. `..`, a root or a
        // drive prefix is refused outright rather than normalized: a path
        // that needs normalizing is not one `npm pack` writes.
        let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
        let mut ordinary = true;
        for comp in path.components() {
            match comp {
                std::path::Component::Normal(name) => parts.push(name),
                std::path::Component::CurDir => {}
                _ => ordinary = false,
            }
        }
        if !ordinary {
            return Err(format!(
                "tarball path traversal: {} escapes {}",
                path.display(),
                dest.display()
            ));
        }

        // npm tarballs wrap everything under one root directory -- `package/`
        // by convention, but npm strips the first component whatever it is
        // called, and so does this. The root itself, and a stray entry beside
        // it, have nothing left and are skipped.
        if parts.len() < 2 {
            continue;
        }
        let rel: PathBuf = parts[1..].iter().collect();

        let target = dest.join(&rel);
        let target_clean = path_clean(&target);

        // Lexical containment, kept as a backstop for what the component
        // check cannot see (on Windows a name like `C:x` re-parses as a
        // drive-relative path once joined).
        if !target_clean.starts_with(&dest_clean) {
            return Err(format!(
                "tarball path traversal: {} escapes {}",
                rel.display(),
                dest.display()
            ));
        }

        // Real containment: nothing is created THROUGH a symlink. For a file
        // or directory that covers the final component too (writing to an
        // existing link follows it); a symlink entry replaces whatever link
        // is at its own path, so only its parents are checked.
        let kind = entry.header().entry_type();
        if symlinks_created {
            let through = if kind == tar::EntryType::Symlink {
                rel.parent()
            } else {
                Some(rel.as_path())
            };
            if through.is_some_and(|p| crosses_symlink(dest, p)) {
                return Err(format!(
                    "tarball path traversal: {} is reached through a symlink",
                    rel.display()
                ));
            }
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }

        // Handle directories and files.
        if kind.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("create dir {}: {e}", target.display()))?;
        } else if kind.is_file() || kind == tar::EntryType::Regular {
            let mut contents = Vec::new();
            entry
                .read_to_end(&mut contents)
                .map_err(|e| format!("read tar entry {}: {e}", rel.display()))?;
            std::fs::write(&target, &contents)
                .map_err(|e| format!("write {}: {e}", target.display()))?;

            // Preserve execute permissions on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(mode) = entry.header().mode() {
                    let _ =
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
                }
            }
        } else if kind == tar::EntryType::Symlink {
            let link_name = match entry.link_name() {
                Ok(Some(name)) => name.into_owned(),
                _ => continue, // unreadable / empty link target: skip
            };
            match create_symlink_entry(&link_name, &target, &dest_clean, &rel)? {
                None => symlinks_created = true,
                Some(note) => notes.push(note),
            }
        }
        // Other entry types (hardlinks, char/block devices, FIFOs) are not
        // produced by `npm pack` and are skipped.
    }
    Ok(())
}

/// Create a symlink entry from an npm tarball, with a target-traversal guard.
///
/// `target` is the link's (already dest-validated) location; `link_name` is the
/// stored target path. The target is resolved relative to the link's own
/// directory and must stay within `dest_clean`: a within-package link (the
/// common case) is created; one that escapes the package -- whether a malicious
/// `../../etc/passwd` or an unsupported cross-package/workspace link -- is
/// skipped with a note rather than created or aborted.
///
/// The containment check is lexical, so the stored target must be one a
/// lexical check can speak for: any `..` comes first (`../lib/x`, never
/// `a/../x`), and it is not absolute. The leading `..` then only climb the
/// link's own parents, which `extract_into` has verified are real
/// directories; a `..` AFTER a name could climb out of whatever link that
/// name turns out to be.
///
/// Unix creates the symlink. On non-Unix it is skipped with a note: Windows
/// symlinks need a privilege normal accounts lack and require knowing file-vs-
/// dir up front, and npm itself shims bins separately rather than relying on
/// tarball symlinks there.
///
/// Returns `Ok(None)` when the link was created and `Ok(Some(note))` when it
/// was skipped. The note goes to the caller's diagnostics -- nothing here
/// prints (see `warnings.rs`: the loader never writes to stderr itself).
fn create_symlink_entry(
    link_name: &Path,
    target: &Path,
    dest_clean: &Path,
    rel: &Path,
) -> Result<Option<String>, String> {
    let mut seen_name = false;
    let mut plain = true;
    for comp in link_name.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => plain &= !seen_name,
            std::path::Component::Normal(_) => seen_name = true,
            _ => plain = false,
        }
    }
    let link_parent = target.parent().unwrap_or(target);
    let resolved = path_clean(&link_parent.join(link_name));
    if !plain || !resolved.starts_with(dest_clean) {
        return Ok(Some(format!(
            "{} -> {} (target outside the package)",
            rel.display(),
            link_name.display()
        )));
    }

    #[cfg(unix)]
    let skipped: Option<String> = {
        // Replace any stale entry so a re-extract is idempotent.
        let _ = std::fs::remove_file(target);
        std::os::unix::fs::symlink(link_name, target).map_err(|e| {
            format!(
                "symlink {} -> {}: {e}",
                target.display(),
                link_name.display()
            )
        })?;
        None
    };
    #[cfg(not(unix))]
    let skipped: Option<String> = Some(format!(
        "{} -> {} (symlinks need a privilege on this platform)",
        rel.display(),
        link_name.display()
    ));
    Ok(skipped)
}

// ── Bin linking ─────────────────────────────────────────────────────────

/// Scan installed packages for `bin` entries and create shims in
/// `node_modules/.bin/`.
///
/// `errors` is appended to (never returned as `Err`) so the caller can
/// classify once. The create-dir and canonicalize-failure paths surface
/// `OAM-PKG0005` warnings, and so does each shim that could not be written
/// or whose `bin` entry is not usable -- as diagnostics, never as prose on
/// stderr, so `oam install --json` stays JSONL.
///
/// Only TOP-LEVEL packages (`node_modules/<name>`) are linked here, as npm
/// does: a nested copy (`node_modules/a/node_modules/semver`) must not
/// replace the command of the top-level package of the same name. (npm gives
/// nested packages a `.bin` beside them; oam does not create those yet.)
pub(crate) fn link_bins(
    node_modules: &Path,
    packages: &[(&str, &LockfileEntry)],
    errors: &mut Vec<Diagnostic>,
) {
    let bin_dir = node_modules.join(".bin");

    // Collect bin entries.
    let mut bins: Vec<(String, std::path::PathBuf)> = Vec::new();

    for (key, entry) in packages {
        // Top-level only: the key is exactly node_modules/<directory name>.
        let dir_name = lockfile_key_package(key);
        if dir_name.is_none() || key.strip_prefix("node_modules/") != dir_name {
            continue;
        }
        let pkg_dir = node_modules.parent().unwrap_or(node_modules).join(key);

        let bin_field = if let Some(ref bin) = entry.bin {
            bin.clone()
        } else {
            // Try reading from the installed package.json.
            let pj_path = pkg_dir.join("package.json");
            let Ok(raw) = std::fs::read_to_string(&pj_path) else {
                continue;
            };
            let Ok(pj) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(bin) = pj.get("bin").cloned() else {
                continue;
            };
            bin
        };

        // The default bin name for a string `bin`: the package's own name
        // without its scope (the lockfile `name` under an alias, else the
        // last segment of the key).
        let pkg_name: &str = entry.name.as_deref().unwrap_or(*key);

        let mut declared: Vec<(&str, &str)> = Vec::new();
        match &bin_field {
            serde_json::Value::String(s) => declared.push((pkg_name, s.as_str())),
            serde_json::Value::Object(map) => {
                for (name, path) in map {
                    if let Some(s) = path.as_str() {
                        declared.push((name.as_str(), s));
                    }
                }
            }
            _ => {}
        }
        for (name, target) in declared {
            // A command name is one file name inside .bin and its target one
            // file inside the package, whatever the manifest spells.
            match (safe_bin_name(name), safe_bin_target(&pkg_dir, target)) {
                (Some(name), Some(target)) => bins.push((name.to_string(), target)),
                _ => errors.push(Diagnostic::new(
                    "OAM-PKG0005",
                    Severity::Warning,
                    Origin::Install,
                    format!(
                        "{key}: bin entry '{name}' -> '{target}' is not usable; no shim created"
                    ),
                )),
            }
        }
    }

    if bins.is_empty() {
        return;
    }

    if let Err(e) = std::fs::create_dir_all(&bin_dir) {
        errors.push(Diagnostic::new(
            "OAM-PKG0005",
            Severity::Warning,
            Origin::Install,
            format!("could not create .bin directory: {e}"),
        ));
        return;
    }

    // Canonicalize the .bin dir once: every shim would otherwise re-stat it
    // via pathdiff, costing 2N syscalls for N bins. If canonicalize fails
    // (e.g. the dir was created above but a perms/symlink race left it
    // unresolvable), we CANNOT safely call pathdiff with an uncanonicalized
    // base — it would produce a wrong relative path that points the shim
    // at garbage. Surface a Severity::Warning and skip the shim loop
    // entirely; install_command in the CLI filters by severity, so a
    // Warning-only diagnostic set still exits 0. (Note: install() classifies
    // ANY non-empty errors list as Partial, but the CLI's severity filter
    // means a warning-only Partial still succeeds end-to-end.)
    let bin_dir_canonical = match std::fs::canonicalize(&bin_dir) {
        Ok(p) => p,
        Err(e) => {
            errors.push(Diagnostic::new(
                "OAM-PKG0005",
                Severity::Warning,
                Origin::Install,
                format!(
                    "could not canonicalize {}: {e} (bin shims will not be created)",
                    bin_dir.display()
                ),
            ));
            return;
        }
    };

    for (name, target) in &bins {
        if let Err(e) = create_bin_shim(&bin_dir, &bin_dir_canonical, name, target) {
            // Non-fatal: warn and continue.
            errors.push(Diagnostic::new(
                "OAM-PKG0005",
                Severity::Warning,
                Origin::Install,
                format!("bin shim for {name}: {e}"),
            ));
        }
    }
}

/// The file name a `bin` key links as: its last path segment, as npm
/// normalizes it (`../../x` and `@scope/x` both name the command `x`). `None`
/// when nothing usable is left.
fn safe_bin_name(name: &str) -> Option<&str> {
    let base = name.rsplit(['/', '\\', ':']).next().unwrap_or(name);
    if matches!(base, "" | "." | "..") {
        None
    } else {
        Some(base)
    }
}

/// A `bin` target as a file inside `pkg_dir`. The value is read as a path
/// relative to the package root: `.` segments drop, `..` climbs no higher
/// than the root (npm's normalization), and a segment carrying a drive colon
/// is refused. `None` when the target names no file.
fn safe_bin_target(pkg_dir: &Path, target: &str) -> Option<PathBuf> {
    let mut parts: Vec<&str> = Vec::new();
    for segment in target.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s if s.contains(':') => return None,
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut out = pkg_dir.to_path_buf();
    out.extend(parts);
    Some(out)
}

fn create_bin_shim(
    bin_dir: &Path,
    bin_dir_canonical: &Path,
    name: &str,
    target: &Path,
) -> Result<(), String> {
    // Compute relative path from .bin to the target.
    let rel = pathdiff(target, bin_dir_canonical);

    #[cfg(windows)]
    {
        // Windows: create a .cmd shim. It starts the script with `node`
        // from PATH (documented in docs/node-divergences.md section 11);
        // which interpreter a shim should name is an open product choice.
        let cmd_path = bin_dir.join(format!("{name}.cmd"));
        let rel_str = rel.to_string_lossy().replace('/', "\\");
        let content = format!(
            "@ECHO off\r\n\
             GOTO start\r\n\
             :find_dp0\r\n\
             SET dp0=%~dp0\r\n\
             EXIT /b\r\n\
             :start\r\n\
             SETLOCAL\r\n\
             CALL :find_dp0\r\n\
             node \"%dp0%\\{rel_str}\" %*\r\n"
        );
        std::fs::write(&cmd_path, content)
            .map_err(|e| format!("write {}: {e}", cmd_path.display()))?;
    }

    #[cfg(not(windows))]
    {
        let link = bin_dir.join(name);
        // Remove stale link/file before creating a new one.
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&rel, &link)
            .map_err(|e| format!("symlink {} -> {}: {e}", link.display(), rel.display()))?;

        // Mark executable.
        use std::os::unix::fs::PermissionsExt;
        if target.is_file() {
            let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755));
        }
    }

    Ok(())
}

/// Simple relative-path computation from `base` to `target`.
///
/// `base_canonical` MUST be the result of `std::fs::canonicalize` (or an
/// equivalent absolute path); it is not re-canonicalized here so callers
/// can share one canonicalization across many calls. Mixing a canonicalized
/// `target` with an uncanonicalized `base` produces a wrong relative path —
/// callers that cannot guarantee canonicalization of the base must fail
/// loudly rather than silently generating garbage shim paths. See
/// `link_bins` for the canonicalize-failure handling.
fn pathdiff(target: &Path, base_canonical: &Path) -> std::path::PathBuf {
    // Canonicalize what we can; fall back to the raw paths.
    let t = std::fs::canonicalize(target)
        .or_else(|_| std::path::absolute(target))
        .unwrap_or_else(|_| target.to_path_buf());
    let b = base_canonical;

    let mut t_parts: Vec<_> = t.components().collect();
    let mut b_parts: Vec<_> = b.components().collect();

    // Drop common prefix.
    while !t_parts.is_empty() && !b_parts.is_empty() && t_parts[0] == b_parts[0] {
        t_parts.remove(0);
        b_parts.remove(0);
    }

    let mut result = std::path::PathBuf::new();
    for _ in &b_parts {
        result.push("..");
    }
    for part in &t_parts {
        result.push(part);
    }
    result
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Lifecycle scripts recorded for a trusted package, to run after the tree and
/// bin shims are in place. `scripts` is `(phase, command)` pairs in npm
/// execution order: preinstall, install, postinstall.
struct PendingScripts {
    pkg_dir: PathBuf,
    pkg_name: String,
    scripts: Vec<(&'static str, String)>,
    /// `node_modules/.oam/scripts-ran/<sanitized-pkg-name>` -- written AFTER
    /// every phase for this package succeeds, so a later warm install skips
    /// re-running (npm run-once semantics).
    marker_path: PathBuf,
}

/// Path of the per-package "scripts already ran" stamp under
/// `node_modules/.oam/scripts-ran/`. The npm package name keys the marker
/// (consistent with how `trust.json` keys packages), but a scoped name like
/// `@scope/name` contains a `/`; we replace it with `+` so the marker is a
/// single flat file (no nested dirs, no `@scope`-dir-vs-file collision).
fn script_marker_path(scripts_ran_dir: &Path, pkg_name: &str) -> PathBuf {
    let safe = pkg_name.replace('/', "+");
    scripts_ran_dir.join(safe)
}

/// After a package is extracted, inspect its `package.json` for lifecycle
/// scripts. Returns `Some(PendingScripts)` ONLY when the package is in the
/// trust list and has at least one lifecycle script (the caller runs them in a
/// later pass). An untrusted package with scripts emits an `OAM-PKG0007`
/// warning and returns `None` -- scripts run only after explicit
/// `oam trust add <pkg>`. No scripts, or unreadable package.json, returns
/// `None` quietly.
///
/// Trust names a PACKAGE, not a directory. `pkg_name` is the directory the
/// package is installed under (it keys the run-once marker); `declared_name`
/// is the package the lockfile says is there -- its `name` under an alias,
/// else the directory name -- and is what the trust list is asked about.
/// Scripts are only ever enqueued when the identities agree: the extracted
/// manifest's own `name` is `declared_name`, and a registry `resolved` URL
/// carries that same name. Otherwise an `OAM-PKG0007` warning says so and the
/// scripts are skipped whatever the trust list holds.
fn check_lifecycle_scripts(
    dest: &Path,
    pkg_name: &str,
    declared_name: &str,
    resolved: Option<&str>,
    scripts_ran_dir: &Path,
    trust: &crate::trust::TrustConfig,
    errors: &mut Vec<Diagnostic>,
) -> Option<PendingScripts> {
    let content = std::fs::read_to_string(dest.join("package.json")).ok()?;
    let json = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    let scripts = json.get("scripts").and_then(|s| s.as_object())?;

    // npm execution order: preinstall -> install -> postinstall.
    const PHASES: [&str; 3] = ["preinstall", "install", "postinstall"];
    let present: Vec<(&'static str, String)> = PHASES
        .iter()
        .filter_map(|phase| {
            scripts
                .get(*phase)
                .and_then(|v| v.as_str())
                .map(|cmd| (*phase, cmd.to_string()))
        })
        .collect();

    if present.is_empty() {
        return None;
    }

    let version = json.get("version").and_then(|v| v.as_str()).unwrap_or("?");
    let script_list = present
        .iter()
        .map(|(p, _)| *p)
        .collect::<Vec<_>>()
        .join(", ");

    let manifest_name = json.get("name").and_then(|v| v.as_str());
    let url_agrees = match resolved {
        Some(url) => resolved_url_names(url, declared_name),
        None => true,
    };
    if manifest_name != Some(declared_name) || !url_agrees {
        errors.push(Diagnostic::new(
            "OAM-PKG0007",
            Severity::Warning,
            Origin::Install,
            format!(
                "{declared_name}@{version} has lifecycle script(s) ({script_list}) -- skipped: the package in {} names itself '{}' and does not match the name and registry path the lockfile gives it, so no trust entry applies",
                dest.display(),
                manifest_name.unwrap_or("?")
            ),
        ));
        return None;
    }

    if !trust.is_trusted(declared_name) {
        errors.push(Diagnostic::new(
            "OAM-PKG0007",
            Severity::Warning,
            Origin::Install,
            format!(
                "{declared_name}@{version} has lifecycle script(s) ({script_list}) -- skipped; run 'oam trust add {declared_name}' to allow"
            ),
        ));
        return None;
    }

    Some(PendingScripts {
        pkg_dir: dest.to_path_buf(),
        pkg_name: declared_name.to_string(),
        scripts: present,
        marker_path: script_marker_path(scripts_ran_dir, pkg_name),
    })
}

/// Whether a `resolved` URL is consistent with the package being `name`. A
/// registry tarball URL is `<registry>/<name>/-/<file>.tgz` (the scope
/// separator sometimes percent-encoded), so when the `/-/` marker is present
/// the path before it must end in `/<name>`. A URL without the marker (a
/// plain tarball host) carries no name to compare and is accepted.
fn resolved_url_names(url: &str, name: &str) -> bool {
    let Some((before, _)) = url.split_once("/-/") else {
        return true;
    };
    let before = before.to_ascii_lowercase().replace("%2f", "/");
    before.ends_with(&format!("/{}", name.to_ascii_lowercase()))
}

/// Run the lifecycle scripts collected from trusted packages, after the full
/// tree and bin shims exist. Each script runs via the platform shell with
/// cwd = the package dir and `node_modules/.bin` prepended to PATH. A failed
/// script is surfaced as an `OAM-PKG0009` error (the install becomes Partial);
/// execution continues with the next package so one bad script doesn't strand
/// the rest. `OAM_IGNORE_SCRIPTS` (1/true/on/yes) is a global kill-switch that
/// skips ALL execution regardless of trust -- like `npm --ignore-scripts`, for
/// CI / hardened environments.
fn run_lifecycle_scripts(
    pending: &[PendingScripts],
    node_modules: &Path,
    errors: &mut Vec<Diagnostic>,
) {
    if pending.is_empty() {
        return;
    }
    if scripts_globally_ignored() {
        for pkg in pending {
            errors.push(Diagnostic::new(
                "OAM-PKG0007",
                Severity::Warning,
                Origin::Install,
                format!(
                    "{}: lifecycle scripts skipped (OAM_IGNORE_SCRIPTS set)",
                    pkg.pkg_name
                ),
            ));
        }
        return;
    }

    let bin_dir = node_modules.join(".bin");
    for pkg in pending {
        let mut all_ok = true;
        for (phase, cmd) in &pkg.scripts {
            if let Err(e) = run_one_script(cmd, phase, &pkg.pkg_dir, &bin_dir) {
                all_ok = false;
                errors.push(Diagnostic::new(
                    "OAM-PKG0009",
                    Severity::Error,
                    Origin::Install,
                    format!("{}: {phase} script failed: {e}", pkg.pkg_name),
                ));
            }
        }
        // Stamp the run-once marker only when EVERY phase succeeded. A failed
        // phase leaves no marker, so the next install re-attempts the scripts
        // (matching the partial-install retry semantics elsewhere in install()).
        // Marker-write failure is non-fatal: worst case a future warm install
        // re-runs an idempotent script -- better than poisoning the install.
        if all_ok {
            if let Some(parent) = pkg.marker_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&pkg.marker_path, b"");
        }
    }
}

/// Whether `OAM_IGNORE_SCRIPTS` requests skipping all lifecycle execution.
fn scripts_globally_ignored() -> bool {
    matches!(
        std::env::var("OAM_IGNORE_SCRIPTS").as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

/// Run a single lifecycle script via the platform shell (`sh -c` /
/// `cmd /d /s /c`), with `node_modules/.bin` prepended to PATH and
/// `npm_lifecycle_event` set.
fn run_one_script(cmd: &str, phase: &str, pkg_dir: &Path, bin_dir: &Path) -> Result<(), String> {
    // Prepend node_modules/.bin to PATH so scripts can call installed bins.
    let mut paths: Vec<PathBuf> = vec![bin_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let new_path = std::env::join_paths(paths).map_err(|e| format!("PATH join: {e}"))?;

    // Windows: the script goes to cmd.exe VERBATIM, wrapped in one pair of
    // quotes that `/s` strips -- what npm and node's `shell: true` do. A
    // plain `.arg(cmd)` would apply the C-runtime quoting rules (an inner `"`
    // becomes `\"`), which cmd.exe does not undo, so a script such as
    // `node -e "require('./x')"` reached its program mangled.
    #[cfg(windows)]
    let (sh, mut command) = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("cmd");
        command.raw_arg("/d /s /c").raw_arg(format!("\"{cmd}\""));
        ("cmd", command)
    };
    #[cfg(not(windows))]
    let (sh, mut command) = {
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(cmd);
        ("sh", command)
    };

    let status = command
        .current_dir(pkg_dir)
        .env("PATH", new_path)
        .env("npm_lifecycle_event", phase)
        .status()
        .map_err(|e| format!("spawn {sh}: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("exit code {:?}", status.code()))
    }
}

fn diag(code: &str, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(code, Severity::Error, Origin::Install, message)
}

/// Normalize a path by collapsing `.` and `..` segments lexically.
///
/// Used by `extract_tarball` to defend against `..`-based path-traversal
/// in tarball entries. The tar crate's `set_path` already rejects `..` in
/// paths the BUILD side constructs, but a tarball arriving over the wire
/// is parsed via raw bytes (see `extract_tarball_rejects_path_traversal`)
/// so a malicious or buggy archive can still carry `..` segments. We
/// can't trust `Path::join` + lexical `starts_with` to catch them, so we
/// clean the target before the prefix check.
fn path_clean(p: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lockfile_v3_basic() {
        let json = r#"{
            "name": "my-project",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "my-project",
                    "version": "1.0.0",
                    "dependencies": {
                        "is-odd": "^3.0.1"
                    }
                },
                "node_modules/is-number": {
                    "version": "6.0.0",
                    "resolved": "https://registry.npmjs.org/is-number/-/is-number-6.0.0.tgz",
                    "integrity": "sha512-Wu1VFo7tl8SNiW2ByqDneXDwJEALjUfOWVslXtKlmXtIikSmEGKRaxDZbiGiBa1v+6A/sLqnJfbQ7Od0dVqGA=="
                },
                "node_modules/is-odd": {
                    "version": "3.0.1",
                    "resolved": "https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz",
                    "integrity": "sha512-CQpnWPrb2wBsXiNHj/HjMhOpD0RGmr5DzTkj0gMzBKB3yLfiliSMiGnCBEKslFqiPQbOlyQQRkSJUS7bV0Nw==",
                    "dependencies": {
                        "is-number": "^6.0.0"
                    }
                }
            }
        }"#;
        let lockfile: Lockfile = serde_json::from_str(json).unwrap();
        assert_eq!(lockfile.lockfile_version, 3);
        assert_eq!(lockfile.packages.len(), 3);
        assert!(lockfile.packages.contains_key(""));
        assert!(lockfile.packages.contains_key("node_modules/is-odd"));

        let is_odd = &lockfile.packages["node_modules/is-odd"];
        assert_eq!(is_odd.version.as_deref(), Some("3.0.1"));
        assert!(is_odd.resolved.as_ref().unwrap().contains("is-odd"));
        assert!(is_odd.integrity.as_ref().unwrap().starts_with("sha512-"));
    }

    #[test]
    fn verify_integrity_sha512() {
        // "hello" hashed with sha512, base64-encoded.
        use sha2::{Digest, Sha512};
        let data = b"hello";
        let hash = Sha512::digest(data);
        let b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, hash.as_slice());
        let sri = format!("sha512-{b64}");
        assert!(verify_integrity(data, &sri).is_ok());

        // Tampered data should fail.
        assert!(verify_integrity(b"world", &sri).is_err());
    }

    #[test]
    fn verify_integrity_sha1() {
        use sha1::Digest;
        let data = b"hello";
        let hash = sha1::Sha1::digest(data);
        let b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, hash.as_slice());
        let sri = format!("sha1-{b64}");
        assert!(verify_integrity(data, &sri).is_ok());
        assert!(verify_integrity(b"world", &sri).is_err());
    }

    #[test]
    fn verify_integrity_invalid_format() {
        assert!(verify_integrity(b"data", "noseparator").is_err());
        assert!(verify_integrity(b"data", "md5-abc").is_err());
    }

    #[test]
    fn install_missing_lockfile_returns_pkg0001() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-install-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let result = install(&tmp, true, false);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].code, "OAM-PKG0001");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_empty_deps_succeeds() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-install-test-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let lockfile = r#"{
            "name": "empty-project",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "empty-project",
                    "version": "1.0.0"
                }
            }
        }"#;
        std::fs::write(tmp.join("package-lock.json"), lockfile).unwrap();
        let result = install(&tmp, true, false);
        match result {
            Ok(InstallOutcome::Complete(summary)) => {
                assert_eq!(summary.packages_installed, 0);
            }
            other => panic!("expected Ok(Complete(_)) with 0 packages; got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // This test makes a real HTTPS call to registry.npmjs.org to exercise
    // the Partial outcome with a package that 404s. It is marked #[ignore]
    // so the default `cargo test` run stays offline and deterministic; run
    // it explicitly with `cargo test -p oam_loader -- --ignored` when
    // network access is available.
    #[test]
    #[ignore = "requires network; run with `cargo test -p oam_loader -- --ignored`"]
    fn install_partial_returns_partial_outcome() {
        // One entry whose `resolved` URL 404s, so the install pass ran but
        // produced zero successful packages. The function must surface this
        // as Ok(Partial { installed: 0, .. }) rather than collapsing it
        // into Err.
        let tmp = std::env::temp_dir().join(format!(
            "oam-install-test-partial-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let lockfile = r#"{
            "name": "partial-project",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "partial-project",
                    "version": "1.0.0"
                },
                "node_modules/does-not-exist": {
                    "version": "1.0.0",
                    "resolved": "https://registry.npmjs.org/does-not-exist/-/does-not-exist-1.0.0.tgz",
                    "integrity": "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
                }
            }
        }"#;
        std::fs::write(tmp.join("package-lock.json"), lockfile).unwrap();
        let result = install(&tmp, true, false);
        match result {
            Ok(InstallOutcome::Partial {
                installed, errors, ..
            }) => {
                assert_eq!(installed, 0);
                assert_eq!(errors[0].code, "OAM-PKG0004");
            }
            other => panic!("expected Ok(Partial {{ installed: 0, .. }}); got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // The installer's GET, against a one-connection loopback server: since
    // #173 it sends no `accept-encoding` (reqwest adds the header only with
    // its "gzip" / "deflate" features, which are gone), and a response that
    // carries `content-encoding: gzip` anyway is handed over STILL ENCODED --
    // verify_integrity then fails with a hash mismatch instead of the body
    // being silently decoded. The registries oam resolves against serve
    // tarballs identity-encoded whatever the offer (registry.npmjs.org,
    // registry.yarnpkg.com, npmmirror's CDN -- measured for #173), so this
    // pins the loud-failure path for a mirror or proxy that encodes
    // unasked, not the common path.
    #[test]
    fn tarball_download_sends_no_accept_encoding_and_passes_an_encoded_body_through() {
        use std::io::{Read, Write};

        let payload = b"the bytes the registry published".to_vec();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&payload).unwrap();
        let encoded = gz.finish().unwrap();
        let expect = encoded.clone();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if conn.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            write!(
                conn,
                "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                 content-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                encoded.len()
            )
            .unwrap();
            conn.write_all(&encoded).unwrap();
            String::from_utf8_lossy(&head).to_ascii_lowercase()
        });

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = http_client_builder().no_proxy().build().unwrap();
        let url = format!("http://127.0.0.1:{port}/pkg/-/pkg-1.0.0.tgz");
        let got = download_with_retry(&rt, &client, &url, 0).unwrap();
        let head = server.join().unwrap();
        assert!(!head.contains("accept-encoding"), "{head}");
        assert_eq!(got, expect);
    }

    #[test]
    fn lockfile_version_2_rejected() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-install-test-v2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let lockfile = r#"{
            "name": "old-project",
            "version": "1.0.0",
            "lockfileVersion": 2,
            "packages": {}
        }"#;
        std::fs::write(tmp.join("package-lock.json"), lockfile).unwrap();
        let result = install(&tmp, true, false);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].code, "OAM-PKG0002");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_tarball_strips_package_prefix() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        // Build a minimal gzipped tarball with a package/ prefix.
        let mut builder = tar::Builder::new(Vec::new());
        let content = b"{\"name\":\"test\",\"version\":\"1.0.0\"}";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path("package/package.json").unwrap();
        header.set_cksum();
        builder.append(&header, &content[..]).unwrap();
        let tar_data = builder.into_inner().unwrap();

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_data).unwrap();
        let gz_data = gz.finish().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "oam-extract-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        extract_tarball(&gz_data, &tmp).unwrap();

        // The package.json should be at tmp/package.json, NOT tmp/package/package.json.
        assert!(tmp.join("package.json").is_file());
        let read_back = std::fs::read_to_string(tmp.join("package.json")).unwrap();
        assert!(read_back.contains("\"name\":\"test\""));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // #18: path-traversal entries must be rejected with an error that names
    // the failure mode, so a malicious or buggy tarball cannot write files
    // outside the destination directory.
    //
    // NOTE on coverage: this test exercises the ABSOLUTE-path case, where
    // `Path::join` replaces the base so the prefix check fails outright.
    // The `..` case is covered separately by
    // `extract_tarball_rejects_dotdot_path_traversal` (`extract_into` refuses
    // any entry path with a `..`, root or prefix component), and an entry
    // reached through a symlink the tarball itself created by
    // `extract_tarball_refuses_an_entry_reached_through_a_symlink`.
    #[test]
    fn extract_tarball_rejects_path_traversal() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        // The `tar` crate's `Header::set_path` rejects `..` and absolute
        // paths, so we build the tar header by hand. Layout: 512-byte header
        // (path in the first 100 bytes, null-terminated), then the entry
        // data, then 512-byte-aligned zero blocks to end-of-archive.
        //
        // We use an absolute path (`/etc/passwd`) which the `tar` crate
        // would refuse via `set_path`; the hand-rolled header stores it
        // raw, and `Path::join` with an absolute path replaces the base,
        // so `target.starts_with(dest)` is false -- the safety check
        // trips.
        let path = b"/etc/passwd\0";
        let mut header = [0u8; 512];
        header[..path.len()].copy_from_slice(path);
        // Mode (octal, ASCII) at offset 100, length 8, null-terminated.
        let mode = b"0000644\0";
        header[100..108].copy_from_slice(mode);
        // Size (octal, ASCII) at offset 124, length 12, null-terminated.
        let size = b"00000000004\0";
        header[124..136].copy_from_slice(size);
        // Magic "ustar\0" (POSIX) at offset 257, length 8.
        header[257..265].copy_from_slice(b"ustar\x0000");
        // Version "00" at offset 265, length 2.
        header[265..267].copy_from_slice(b"00");
        // Type flag at offset 156: '0' = regular file.
        header[156] = b'0';
        // Compute the header checksum (sum of all bytes treating the chksum
        // field itself as eight spaces).
        let chksum_field = &mut header[148..156];
        for b in chksum_field.iter_mut() {
            *b = b' ';
        }
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        let chksum = format!("{:06o}\0 ", sum);
        header[148..156].copy_from_slice(chksum.as_bytes());

        let content = b"root";
        // Pad content to 512 bytes.
        let mut data = [0u8; 512];
        data[..content.len()].copy_from_slice(content);

        // Two 512-byte zero blocks mark end-of-archive.
        let mut tar_bytes = Vec::new();
        tar_bytes.extend_from_slice(&header);
        tar_bytes.extend_from_slice(&data);
        tar_bytes.extend_from_slice(&[0u8; 1024]);

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        let gz_data = gz.finish().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "oam-extract-traversal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let result = extract_tarball(&gz_data, &tmp);
        assert!(result.is_err(), "expected path-traversal rejection");
        let err = result.unwrap_err();
        assert!(
            err.contains("path traversal") || err.contains("escapes"),
            "error should mention path traversal or escapes; got: {err}"
        );

        // The escape file must NOT exist at the absolute path or inside dest.
        assert!(!tmp.join("etc/passwd").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Closes the `..`-segment gap that the lexical `starts_with` check
    // missed: an entry `package/../../escape.txt` joined to a non-root
    // `dest` lexically starts with `dest` but resolves outside it. The
    // path_clean + dest_clean normalization catches it.
    #[test]
    fn extract_tarball_rejects_dotdot_path_traversal() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        // Hand-build the tar header as a raw 512-byte buffer (the same
        // pattern the existing path-traversal test uses). Path bytes
        // are the leading-NUL-terminated string stored at offset 0.
        let path_bytes = b"package/../../escape.txt\0";
        let mut header = [0u8; 512];
        let plen = path_bytes.len().min(100);
        header[..plen].copy_from_slice(&path_bytes[..plen]);
        // Size field at offset 124, octal ASCII, NUL-terminated.
        let size = format!("{:011o}\0", 5);
        header[124..136].copy_from_slice(size.as_bytes());
        // Magic "ustar\0" at offset 257, length 8.
        header[257..265].copy_from_slice(b"ustar\x0000");
        // Version "00" at offset 265, length 2.
        header[265..267].copy_from_slice(b"00");
        // Type flag at offset 156: '0' = regular file.
        header[156] = b'0';
        // Compute the header checksum (sum of all bytes treating the chksum
        // field itself as eight spaces).
        let chksum_field = &mut header[148..156];
        for b in chksum_field.iter_mut() {
            *b = b' ';
        }
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        let chksum = format!("{:06o}\0 ", sum);
        header[148..156].copy_from_slice(chksum.as_bytes());

        let content = b"hello";
        let mut data = [0u8; 512];
        data[..content.len()].copy_from_slice(content);

        let mut tar_bytes = Vec::new();
        tar_bytes.extend_from_slice(&header);
        tar_bytes.extend_from_slice(&data);
        tar_bytes.extend_from_slice(&[0u8; 1024]);

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        let gz_data = gz.finish().unwrap();

        // Non-root dest matters: if dest is `/`, `..` segments stay
        // inside it. We want a dest whose parent is genuinely outside it.
        let tmp = std::env::temp_dir().join(format!(
            "oam-extract-dotdot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let result = extract_tarball(&gz_data, &tmp);
        assert!(result.is_err(), "expected .. traversal rejection");
        let err = result.unwrap_err();
        assert!(
            err.contains("path traversal") || err.contains("escapes"),
            "error should mention path traversal or escapes; got: {err}"
        );

        // The escape file must NOT have been written outside dest. With
        // `package/../../escape.txt` and a tmp under temp_dir, the bad
        // target lands at <temp_dir>/escape.txt -- two levels up from tmp.
        let bad_target = tmp
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("escape.txt"));
        if let Some(ref p) = bad_target {
            assert!(!p.exists(), "escape file leaked to {}", p.display());
        }
        let _ = std::fs::remove_dir_all(&tmp);
        if let Some(p) = bad_target {
            let _ = std::fs::remove_file(p);
        }
    }

    // A3: a within-package symlink entry is created on Unix (no longer dropped)
    // and skipped on platforms where symlinks need a privilege (Windows).
    #[test]
    fn extract_tarball_creates_within_package_symlink() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut builder = tar::Builder::new(Vec::new());

        let content = b"ok";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path("package/real.txt").unwrap();
        header.set_cksum();
        builder.append(&header, &content[..]).unwrap();

        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_size(0);
        link_header.set_path("package/link.txt").unwrap();
        link_header.set_link_name("real.txt").unwrap();
        link_header.set_cksum();
        builder.append(&link_header, std::io::empty()).unwrap();

        let tar_data = builder.into_inner().unwrap();

        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_data).unwrap();
        let gz_data = gz.finish().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "oam-extract-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let result = extract_tarball(&gz_data, &tmp);
        assert!(
            result.is_ok(),
            "symlink extract should succeed; got {result:?}"
        );
        assert!(tmp.join("real.txt").is_file());

        #[cfg(unix)]
        {
            let link = tmp.join("link.txt");
            assert!(
                std::fs::symlink_metadata(&link)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "link.txt should be a symlink on unix"
            );
            // The link resolves to real.txt's contents.
            assert_eq!(std::fs::read_to_string(&link).unwrap(), "ok");
        }
        #[cfg(not(unix))]
        {
            // Skipped on platforms without unprivileged symlinks.
            assert!(!tmp.join("link.txt").exists());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A4: a TRUSTED package with a lifecycle script yields PendingScripts (to
    // run later) and emits no warning; an UNTRUSTED one warns + returns None.
    #[test]
    fn check_lifecycle_scripts_trusted_vs_untrusted() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-a4-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pkg = tmp.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"p","version":"1.0.0","scripts":{"postinstall":"echo hi","build":"tsc"}}"#,
        )
        .unwrap();

        let scripts_ran_dir = tmp.join("node_modules").join(".oam").join("scripts-ran");

        // Untrusted: warns, returns None.
        let empty = crate::trust::TrustConfig::default();
        let mut errors = Vec::new();
        assert!(
            check_lifecycle_scripts(&pkg, "p", "p", None, &scripts_ran_dir, &empty, &mut errors)
                .is_none()
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "OAM-PKG0007");

        // Trusted: returns the lifecycle scripts only (not "build"), no warning.
        let mut trusted = crate::trust::TrustConfig::default();
        trusted.add("p");
        let mut errors2 = Vec::new();
        let pending = check_lifecycle_scripts(
            &pkg,
            "p",
            "p",
            None,
            &scripts_ran_dir,
            &trusted,
            &mut errors2,
        )
        .expect("trusted pkg with scripts yields pending");
        assert!(errors2.is_empty(), "trusted: no warning; got {errors2:?}");
        assert_eq!(
            pending.scripts.len(),
            1,
            "only lifecycle scripts, not build"
        );
        assert_eq!(pending.scripts[0].0, "postinstall");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A4: run_lifecycle_scripts actually executes a trusted package's script
    // (here a postinstall that writes a marker file in the package dir).
    #[test]
    fn run_lifecycle_scripts_executes_trusted() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-a4-run-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nm = tmp.join("node_modules");
        let pkg_dir = nm.join("trusted-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();

        // `echo ... > marker.txt` works under both `sh -c` and `cmd /C`.
        let scripts_ran_dir = nm.join(".oam").join("scripts-ran");
        let stamp = script_marker_path(&scripts_ran_dir, "trusted-pkg");
        let pending = vec![PendingScripts {
            pkg_dir: pkg_dir.clone(),
            pkg_name: "trusted-pkg".to_string(),
            scripts: vec![("postinstall", "echo done > marker.txt".to_string())],
            marker_path: stamp.clone(),
        }];
        let mut errors = Vec::new();
        run_lifecycle_scripts(&pending, &nm, &mut errors);

        assert!(errors.is_empty(), "script should succeed; got {errors:?}");
        assert!(
            pkg_dir.join("marker.txt").is_file(),
            "postinstall should have created the marker"
        );
        // The run-once stamp must be written after a successful run, so a later
        // warm install skips re-running the script.
        assert!(
            stamp.is_file(),
            "scripts-ran stamp should exist after a successful run"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A3 guard: a symlink whose target escapes the package is skipped (not
    // created, not a hard error) -- covers both malicious `../../etc/passwd`
    // links and unsupported cross-package links.
    #[test]
    fn extract_tarball_skips_escaping_symlink_target() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut builder = tar::Builder::new(Vec::new());

        // A regular file so the archive is non-empty.
        let content = b"ok";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path("package/real.txt").unwrap();
        header.set_cksum();
        builder.append(&header, &content[..]).unwrap();

        // A symlink pointing well outside the package.
        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_size(0);
        link_header.set_path("package/escape").unwrap();
        link_header.set_link_name("../../../../etc/passwd").unwrap();
        link_header.set_cksum();
        builder.append(&link_header, std::io::empty()).unwrap();

        let tar_data = builder.into_inner().unwrap();
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&tar_data).unwrap();
        let gz_data = gz.finish().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "oam-extract-symlink-escape-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Escaping target is skipped, not an error; extraction still succeeds.
        let result = extract_tarball(&gz_data, &tmp);
        assert!(
            result.is_ok(),
            "escaping symlink should be skipped; got {result:?}"
        );
        assert!(tmp.join("real.txt").is_file());
        assert!(
            std::fs::symlink_metadata(tmp.join("escape")).is_err(),
            "escaping symlink must not be created"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // #21: link_bins should create shims for every entry in an object-form
    // `bin` field (TypeScript-style). Driving link_bins directly avoids the
    // fetch/extract network path and lets the test stay offline.
    #[test]
    fn link_bins_object_form_creates_shims() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-linkbins-object-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Fake "installed" package: node_modules/typescript/ with the bin
        // scripts the shim should point at.
        let pkg_dir = tmp.join("node_modules/typescript");
        std::fs::create_dir_all(pkg_dir.join("bin")).unwrap();
        std::fs::write(pkg_dir.join("bin/tsc"), "#!/bin/sh\necho tsc\n").unwrap();
        std::fs::write(pkg_dir.join("bin/tsserver"), "#!/bin/sh\necho tsserver\n").unwrap();

        // Lockfile entry carries the bin object directly (npm v3 mirrors it).
        let entry = LockfileEntry {
            version: Some("5.4.5".into()),
            resolved: Some("https://example.invalid/typescript-5.4.5.tgz".into()),
            integrity: None,
            bin: Some(serde_json::json!({
                "tsc": "./bin/tsc",
                "tsserver": "./bin/tsserver"
            })),
            ..Default::default()
        };
        let packages: Vec<(&str, &LockfileEntry)> = vec![(
            "node_modules/typescript",
            // SAFETY: `entry` is local; nothing in the test outlives it.
            &entry,
        )];

        let mut errors: Vec<Diagnostic> = Vec::new();
        link_bins(&tmp.join("node_modules"), &packages, &mut errors);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");

        let bin_dir = tmp.join("node_modules/.bin");
        #[cfg(windows)]
        {
            assert!(bin_dir.join("tsc.cmd").is_file());
            assert!(bin_dir.join("tsserver.cmd").is_file());
        }
        #[cfg(not(windows))]
        {
            assert!(
                std::fs::symlink_metadata(bin_dir.join("tsc"))
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "tsc should be a symlink"
            );
            assert!(
                std::fs::symlink_metadata(bin_dir.join("tsserver"))
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false),
                "tsserver should be a symlink"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // #22: a legacy array-form `bin` field must be silently dropped (the
    // `_ => {}` arm in link_bins). No shim files should be created.
    #[test]
    fn link_bins_array_form_silently_dropped() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-linkbins-array-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // The package itself is irrelevant for the array path (link_bins
        // short-circuits on `_ => {}`), but a pkg.json with a string bin
        // would push entries even if `entry.bin` is the array — entry.bin
        // is read first. So we use the entry.bin = array path.
        let pkg_dir = tmp.join("node_modules/legacy-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"legacy-pkg","version":"1.0.0","bin":["cmd1","cmd2"]}"#,
        )
        .unwrap();

        let entry = LockfileEntry {
            version: Some("1.0.0".into()),
            resolved: Some("https://example.invalid/legacy-pkg-1.0.0.tgz".into()),
            integrity: None,
            bin: Some(serde_json::json!(["cmd1", "cmd2"])),
            ..Default::default()
        };
        let packages: Vec<(&str, &LockfileEntry)> = vec![("node_modules/legacy-pkg", &entry)];

        let mut errors: Vec<Diagnostic> = Vec::new();
        link_bins(&tmp.join("node_modules"), &packages, &mut errors);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");

        // The .bin directory should not even have been created (bins was
        // empty after the array arm), and no shim files exist.
        let bin_dir = tmp.join("node_modules/.bin");
        assert!(!bin_dir.exists(), ".bin dir should not have been created");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // #24: install with `frozen = false` must fail fast with OAM-PKG0006
    // and a message that names the flag, so users see why the install was
    // refused rather than getting a generic error.
    #[test]
    fn install_non_frozen_returns_pkg0006() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-install-test-nonfrozen-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Lockfile content is irrelevant — the non-frozen check fires first.
        let result = install(&tmp, false, false);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].code, "OAM-PKG0006");
        assert!(
            errors[0].message.contains("frozen-lockfile"),
            "message should mention frozen-lockfile; got: {}",
            errors[0].message
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A fresh, empty directory under the system temp dir.
    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oam-install-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A gzipped tarball of regular files, paths taken as written.
    fn tarball_of(files: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;

        let mut builder = tar::Builder::new(Vec::new());
        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_path(path).unwrap();
            header.set_cksum();
            builder.append(&header, content.as_bytes()).unwrap();
        }
        let tar_data = builder.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_data).unwrap();
        gz.finish().unwrap()
    }

    // A lockfile key is only ever a package path inside the project.
    #[test]
    fn lockfile_key_package_accepts_only_package_paths() {
        assert_eq!(lockfile_key_package("node_modules/foo"), Some("foo"));
        assert_eq!(
            lockfile_key_package("node_modules/@scope/bar"),
            Some("@scope/bar")
        );
        assert_eq!(
            lockfile_key_package("node_modules/react/node_modules/scheduler"),
            Some("scheduler")
        );
        assert_eq!(
            lockfile_key_package("packages/app/node_modules/@scope/bar"),
            Some("@scope/bar")
        );
        for refused in [
            "",
            "foo",
            "packages/app",
            "../node_modules/foo",
            "node_modules/../../foo",
            "node_modules/foo/../../../bar",
            "/node_modules/foo",
            "node_modules//foo",
            "node_modules/foo/",
            "node_modules/.bin",
            "node_modules/@scope",
            "node_modules/@/foo",
            "node_modules/@scope/.hidden",
            "node_modules/foo/bar",
            "node_modules\\foo",
            "C:/node_modules/foo",
            // The last "node_modules" segment anchors; nothing follows it.
            "node_modules/node_modules",
        ] {
            assert_eq!(lockfile_key_package(refused), None, "{refused:?}");
        }
    }

    // npm-install-checks' list rule, for the `os` and `cpu` fields.
    #[test]
    fn platform_list_allows_follows_npm() {
        use serde_json::json;

        assert!(platform_list_allows(None, "linux"));
        assert!(platform_list_allows(Some(&json!([])), "linux"));
        assert!(platform_list_allows(Some(&json!(["any"])), "linux"));
        assert!(platform_list_allows(Some(&json!("linux")), "linux"));
        assert!(platform_list_allows(
            Some(&json!(["darwin", "linux"])),
            "linux"
        ));
        assert!(!platform_list_allows(Some(&json!(["darwin"])), "linux"));
        assert!(platform_list_allows(Some(&json!(["!win32"])), "linux"));
        assert!(!platform_list_allows(Some(&json!(["!win32"])), "win32"));
        assert!(!platform_list_allows(
            Some(&json!(["linux", "!linux"])),
            "linux"
        ));
    }

    // Link entries, entries for another platform and non-URL `resolved`
    // values are settled from the lockfile alone: nothing is fetched (the
    // hosts below do not resolve; a fetch attempt would add a "was not
    // installed" diagnostic to the two this test expects).
    #[test]
    fn install_settles_link_platform_and_non_url_entries_without_fetching() {
        let tmp = fresh_dir("no-fetch");
        let lockfile = r#"{
            "name": "ws-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "ws-root", "version": "1.0.0", "workspaces": ["packages/*"]},
                "node_modules/ws-pkg": {"resolved": "packages/ws-pkg", "link": true},
                "packages/ws-pkg": {"name": "ws-pkg", "version": "1.0.0"},
                "node_modules/other-platform": {
                    "version": "1.0.0",
                    "resolved": "https://registry.example.invalid/other-platform/-/other-platform-1.0.0.tgz",
                    "optional": true,
                    "os": ["no-such-os"]
                },
                "node_modules/other-cpu": {
                    "version": "1.0.0",
                    "resolved": "https://registry.example.invalid/other-cpu/-/other-cpu-1.0.0.tgz",
                    "optional": true,
                    "cpu": ["no-such-cpu"]
                },
                "node_modules/from-file": {
                    "version": "1.0.0",
                    "resolved": "file:vendor/from-file-1.0.0.tgz"
                }
            }
        }"#;
        std::fs::write(tmp.join("package-lock.json"), lockfile).unwrap();

        let (installed, errors) = match install(&tmp, true, false) {
            Ok(InstallOutcome::Partial {
                installed, errors, ..
            }) => (installed, errors),
            other => panic!("expected a Partial outcome; got {other:?}"),
        };
        assert_eq!(installed, 0);

        let warnings: Vec<&Diagnostic> = errors
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();
        let hard: Vec<&Diagnostic> = errors
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(warnings.len(), 1, "{errors:?}");
        assert!(
            warnings[0].message.contains("node_modules/ws-pkg")
                && warnings[0].message.contains("not linked"),
            "{}",
            warnings[0].message
        );
        assert_eq!(hard.len(), 1, "{errors:?}");
        assert_eq!(hard[0].code, "OAM-PKG0004");
        assert!(
            hard[0].message.contains("node_modules/from-file")
                && hard[0].message.contains("unsupported `resolved`"),
            "{}",
            hard[0].message
        );
        assert!(!tmp.join("node_modules/other-platform").exists());
        assert!(!tmp.join("node_modules/other-cpu").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A key that is not a package path is refused before anything is joined
    // to the project directory, and the directory it points at is untouched.
    #[test]
    fn install_refuses_a_key_outside_node_modules() {
        let tmp = fresh_dir("bad-key");
        let project = tmp.join("project");
        let sibling = tmp.join("sibling");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("keep.txt"), "keep").unwrap();
        let lockfile = r#"{
            "name": "bad-key",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "bad-key", "version": "1.0.0"},
                "../sibling": {
                    "version": "1.0.0",
                    "resolved": "https://registry.example.invalid/sibling/-/sibling-1.0.0.tgz"
                }
            }
        }"#;
        std::fs::write(project.join("package-lock.json"), lockfile).unwrap();

        let (installed, errors) = match install(&project, true, false) {
            Ok(InstallOutcome::Partial {
                installed, errors, ..
            }) => (installed, errors),
            other => panic!("expected a Partial outcome; got {other:?}"),
        };
        assert_eq!(installed, 0);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, "OAM-PKG0004");
        assert!(
            errors[0].message.contains("lockfile key"),
            "{}",
            errors[0].message
        );
        assert!(sibling.join("keep.txt").is_file());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // "Installed" means the lockfile's version, not merely a package.json.
    #[test]
    fn installed_version_differs_compares_manifest_to_lockfile() {
        let tmp = fresh_dir("version");
        let manifest = tmp.join("package.json");
        std::fs::write(&manifest, r#"{"name":"p","version":"1.0.0"}"#).unwrap();
        assert!(!installed_version_differs(&manifest, Some("1.0.0")));
        assert!(installed_version_differs(&manifest, Some("1.1.0")));
        assert!(!installed_version_differs(&manifest, None));

        std::fs::write(&manifest, r#"{"name":"p","version":"v1.0.0"}"#).unwrap();
        assert!(!installed_version_differs(&manifest, Some("1.0.0")));

        // Unknown is "same": no version, unparseable, missing.
        std::fs::write(&manifest, r#"{"name":"p"}"#).unwrap();
        assert!(!installed_version_differs(&manifest, Some("1.0.0")));
        std::fs::write(&manifest, "not json").unwrap();
        assert!(!installed_version_differs(&manifest, Some("1.0.0")));
        assert!(!installed_version_differs(
            &tmp.join("absent.json"),
            Some("1.0.0")
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A stale install is replaced by the extract; without `replace` a
    // complete package at dest is a peer's copy and wins.
    #[test]
    fn extract_replaces_a_stale_package_only_when_asked() {
        let tmp = fresh_dir("replace");
        let dest = tmp.join("node_modules").join("p");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(
            dest.join("package.json"),
            r#"{"name":"p","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(dest.join("old.txt"), "old").unwrap();
        let new = tarball_of(&[("package/package.json", r#"{"name":"p","version":"1.1.0"}"#)]);

        extract_tarball_with(&new, &dest, false, &mut Vec::new()).unwrap();
        assert!(dest.join("old.txt").is_file(), "a peer's copy is kept");

        extract_tarball_with(&new, &dest, true, &mut Vec::new()).unwrap();
        assert!(!dest.join("old.txt").exists(), "the stale tree is gone");
        let manifest = std::fs::read_to_string(dest.join("package.json")).unwrap();
        assert!(manifest.contains("1.1.0"), "{manifest}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // The prune pass removes unlisted PACKAGES (top-level, scoped, nested)
    // and nothing else.
    #[test]
    fn prune_unlisted_removes_only_packages_the_lockfile_does_not_name() {
        let tmp = fresh_dir("prune");
        let nm = tmp.join("node_modules");
        let package = |rel: &str| {
            let dir = nm.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("package.json"),
                r#"{"name":"x","version":"1.0.0"}"#,
            )
            .unwrap();
        };
        package("keep");
        package("keep/node_modules/stale-nested");
        package("keep/node_modules/kept-nested");
        package("gone");
        package("@s/kept");
        package("@s/dropped");
        std::fs::create_dir_all(nm.join(".bin")).unwrap();
        std::fs::write(nm.join(".bin").join("tool"), "x").unwrap();
        std::fs::create_dir_all(nm.join(".oam").join("precompile")).unwrap();
        std::fs::create_dir_all(nm.join("not-a-package")).unwrap();
        std::fs::write(nm.join("not-a-package").join("notes.txt"), "x").unwrap();

        let known: HashSet<&str> = [
            "node_modules/keep",
            "node_modules/keep/node_modules/kept-nested",
            "node_modules/@s/kept",
        ]
        .into_iter()
        .collect();
        prune_unlisted(&nm, "node_modules", &known);

        assert!(nm.join("keep/package.json").is_file());
        assert!(
            nm.join("keep/node_modules/kept-nested/package.json")
                .is_file()
        );
        assert!(!nm.join("keep/node_modules/stale-nested").exists());
        assert!(!nm.join("gone").exists());
        assert!(nm.join("@s/kept/package.json").is_file());
        assert!(!nm.join("@s/dropped").exists());
        assert!(nm.join(".bin").join("tool").is_file());
        assert!(nm.join(".oam").join("precompile").is_dir());
        assert!(nm.join("not-a-package").join("notes.txt").is_file());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // A multi-hash SRI string verifies against its strongest algorithm.
    #[test]
    fn verify_integrity_multiple_hashes() {
        let data = b"hello";
        let b64 = |bytes: &[u8]| {
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
        };
        let sha512 = {
            use sha2::{Digest, Sha512};
            b64(Sha512::digest(data).as_slice())
        };
        let sha256 = {
            use sha2::{Digest, Sha256};
            b64(Sha256::digest(data).as_slice())
        };
        let sha1 = {
            use sha1::Digest;
            b64(sha1::Sha1::digest(data).as_slice())
        };
        let wrong512 = {
            use sha2::{Digest, Sha512};
            b64(Sha512::digest(b"world").as_slice())
        };

        // Both present: fine, in either order, with an options suffix.
        assert!(verify_integrity(data, &format!("sha512-{sha512} sha1-{sha1}")).is_ok());
        assert!(verify_integrity(data, &format!("sha1-{sha1} sha512-{sha512}?opt")).is_ok());
        assert!(verify_integrity(data, &format!("sha256-{sha256}")).is_ok());
        // Two hashes of the strongest algorithm: either may match.
        assert!(verify_integrity(data, &format!("sha512-{wrong512} sha512-{sha512}")).is_ok());
        // The strongest algorithm decides: a matching sha1 does not rescue a
        // sha512 that does not match.
        let err = verify_integrity(data, &format!("sha512-{wrong512} sha1-{sha1}")).unwrap_err();
        assert!(err.contains("sha512 integrity mismatch"), "{err}");
        // An unknown algorithm beside a known one is ignored.
        assert!(verify_integrity(data, &format!("md5-abc sha1-{sha1}")).is_ok());
        assert!(verify_integrity(data, "").is_err());
    }

    // npm strips the tarball's first path component whatever it is called.
    #[test]
    fn extract_tarball_strips_any_root_directory() {
        let gz = tarball_of(&[
            ("pkg-root/package.json", r#"{"name":"t","version":"1.0.0"}"#),
            ("pkg-root/lib/index.js", "module.exports = 1;\n"),
            ("stray-file-beside-the-root", "ignored"),
        ]);
        let tmp = fresh_dir("any-root");
        extract_tarball(&gz, &tmp).unwrap();
        assert!(tmp.join("package.json").is_file());
        assert!(tmp.join("lib").join("index.js").is_file());
        assert!(!tmp.join("pkg-root").exists());
        assert!(!tmp.join("stray-file-beside-the-root").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Containment is real, not only lexical: an entry whose path runs
    // through a symlink an earlier entry created is refused, and a link
    // whose target climbs after a name is not created at all.
    #[cfg(unix)]
    #[test]
    fn extract_tarball_refuses_an_entry_reached_through_a_symlink() {
        use std::io::Write;

        let build = |link_target: &str, file_path: &str| {
            let mut builder = tar::Builder::new(Vec::new());
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            link.set_path("package/a/up").unwrap();
            link.set_link_name(link_target).unwrap();
            link.set_cksum();
            builder.append(&link, std::io::empty()).unwrap();

            let content = b"x";
            let mut file = tar::Header::new_gnu();
            file.set_size(content.len() as u64);
            file.set_entry_type(tar::EntryType::Regular);
            file.set_path(file_path).unwrap();
            file.set_cksum();
            builder.append(&file, &content[..]).unwrap();

            let tar_data = builder.into_inner().unwrap();
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gz.write_all(&tar_data).unwrap();
            gz.finish().unwrap()
        };

        // `a/up -> ..` stays inside the package and is created; a file
        // addressed through it is refused.
        let tmp = fresh_dir("through-link");
        let dest = tmp.join("pkg");
        std::fs::create_dir_all(&dest).unwrap();
        let err = extract_tarball(&build("..", "package/a/up/through.txt"), &dest).unwrap_err();
        assert!(err.contains("through a symlink"), "{err}");
        assert!(!dest.join("through.txt").exists());
        assert!(!tmp.join("through.txt").exists());

        // A target with `..` after a name is skipped with a note, so the
        // file after it is an ordinary write into a real directory.
        let dest2 = tmp.join("pkg2");
        std::fs::create_dir_all(&dest2).unwrap();
        let mut notes = Vec::new();
        extract_tarball_with(
            &build("b/../..", "package/plain.txt"),
            &dest2,
            false,
            &mut notes,
        )
        .unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(std::fs::symlink_metadata(dest2.join("a").join("up")).is_err());
        assert!(dest2.join("plain.txt").is_file());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Only top-level packages link into the root .bin, a bin name is one
    // file name inside it, and a bin target stays inside its package.
    #[test]
    fn link_bins_top_level_only_and_contained() {
        let tmp = fresh_dir("linkbins-contained");
        let nm = tmp.join("node_modules");
        let file = |rel: &str| {
            let path = nm.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
        };
        file("semver/bin/top.js");
        file("zeta/node_modules/semver/bin/nested.js");
        file("odd/run.js");
        file("odd/inside.js");

        let top = LockfileEntry {
            bin: Some(serde_json::json!({"semver": "bin/top.js"})),
            ..Default::default()
        };
        let nested = LockfileEntry {
            bin: Some(serde_json::json!({"semver": "bin/nested.js"})),
            ..Default::default()
        };
        let odd = LockfileEntry {
            bin: Some(serde_json::json!({
                "../../escape": "run.js",
                "climb": "../../inside.js",
                "..": "run.js"
            })),
            ..Default::default()
        };
        let packages: Vec<(&str, &LockfileEntry)> = vec![
            ("node_modules/odd", &odd),
            ("node_modules/semver", &top),
            ("node_modules/zeta/node_modules/semver", &nested),
        ];
        let mut errors: Vec<Diagnostic> = Vec::new();
        link_bins(&nm, &packages, &mut errors);

        // The one unusable entry ("..") is a warning, not prose on stderr.
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, "OAM-PKG0005");
        assert!(errors[0].severity == Severity::Warning);

        let bin_dir = nm.join(".bin");
        // What a shim points at, as text.
        let shim = |name: &str| -> String {
            #[cfg(windows)]
            let text = std::fs::read_to_string(bin_dir.join(format!("{name}.cmd"))).unwrap();
            #[cfg(not(windows))]
            let text = std::fs::read_link(bin_dir.join(name))
                .unwrap()
                .to_string_lossy()
                .into_owned();
            text
        };
        assert!(shim("semver").contains("top.js"), "{}", shim("semver"));
        assert!(!shim("semver").contains("nested.js"));
        assert!(shim("escape").contains("run.js"));
        assert!(shim("climb").contains("inside.js"));
        // Nothing was written above .bin.
        assert!(!tmp.join("escape").exists());
        assert!(!tmp.join("escape.cmd").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Trust names a package: scripts run only when the directory's lockfile
    // name, the manifest's own name and a registry URL's path all agree.
    #[test]
    fn check_lifecycle_scripts_requires_matching_identity() {
        let tmp = fresh_dir("identity");
        let pkg = tmp.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"other","version":"1.0.0","scripts":{"postinstall":"echo hi"}}"#,
        )
        .unwrap();
        let scripts_ran_dir = tmp.join("node_modules").join(".oam").join("scripts-ran");
        let mut trust = crate::trust::TrustConfig::default();
        trust.add("p");
        trust.add("other");

        // Installed as "p" with no lockfile name: the manifest disagrees.
        let mut errors = Vec::new();
        assert!(
            check_lifecycle_scripts(&pkg, "p", "p", None, &scripts_ran_dir, &trust, &mut errors)
                .is_none()
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, "OAM-PKG0007");
        assert!(errors[0].message.contains("names itself 'other'"));

        // Lockfile name agrees with the manifest, the registry path does not.
        let mut errors = Vec::new();
        assert!(
            check_lifecycle_scripts(
                &pkg,
                "p",
                "other",
                Some("https://registry.example.invalid/p/-/p-1.0.0.tgz"),
                &scripts_ran_dir,
                &trust,
                &mut errors,
            )
            .is_none()
        );
        assert_eq!(errors.len(), 1, "{errors:?}");

        // All three agree: the scripts are enqueued under the real name, and
        // the run-once marker stays keyed on the directory.
        let mut errors = Vec::new();
        let pending = check_lifecycle_scripts(
            &pkg,
            "p",
            "other",
            Some("https://registry.example.invalid/other/-/other-1.0.0.tgz"),
            &scripts_ran_dir,
            &trust,
            &mut errors,
        )
        .expect("consistent identity and trusted");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(pending.pkg_name, "other");
        assert_eq!(
            pending.marker_path,
            script_marker_path(&scripts_ran_dir, "p")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolved_url_names_reads_the_registry_path() {
        let base = "https://registry.example.invalid";
        assert!(resolved_url_names(
            &format!("{base}/foo/-/foo-1.0.0.tgz"),
            "foo"
        ));
        assert!(resolved_url_names(
            &format!("{base}/@s/foo/-/foo-1.0.0.tgz"),
            "@s/foo"
        ));
        assert!(resolved_url_names(
            &format!("{base}/@s%2Ffoo/-/foo-1.0.0.tgz"),
            "@s/foo"
        ));
        assert!(!resolved_url_names(
            &format!("{base}/bar/-/bar-1.0.0.tgz"),
            "foo"
        ));
        assert!(!resolved_url_names(
            &format!("{base}/xfoo/-/xfoo-1.0.0.tgz"),
            "foo"
        ));
        // No registry marker: nothing to compare.
        assert!(resolved_url_names(
            &format!("{base}/tarballs/anything.tgz"),
            "foo"
        ));
    }

    // A script's own double quotes reach the shell as written, on both
    // platforms (cmd.exe gets the line verbatim, not C-runtime-escaped).
    #[test]
    fn run_one_script_passes_double_quotes_through() {
        let tmp = fresh_dir("script-quotes");
        run_one_script(
            "echo \"a b\" > marker.txt",
            "postinstall",
            &tmp,
            &tmp.join("node_modules").join(".bin"),
        )
        .unwrap();
        let got = std::fs::read_to_string(tmp.join("marker.txt")).unwrap();
        assert!(got.contains("a b"), "{got:?}");
        assert!(!got.contains('\\'), "{got:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
