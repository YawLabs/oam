//! Lockfile-driven package installer (MVP: frozen-lockfile mode only).
//!
//! Reads npm's `package-lock.json` v3, downloads tarballs from the resolved
//! URLs, verifies SRI integrity hashes, extracts into `node_modules`, and
//! creates `.bin` shims. Equivalent to `npm ci` for oam projects.

use std::collections::HashMap;
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
#[derive(Debug, Deserialize)]
pub(crate) struct LockfileEntry {
    #[allow(dead_code)]
    version: Option<String>,
    resolved: Option<String>,
    integrity: Option<String>,
    #[allow(dead_code)]
    dev: Option<bool>,
    #[allow(dead_code)]
    optional: Option<bool>,
    /// The `bin` field inside the lockfile entry (npm v3 mirrors it from
    /// the package's own package.json).
    bin: Option<serde_json::Value>,
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
///   extract failure) and `OAM-PKG0005` (bin shim warning).
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
    // Install mutates the project layout (writes node_modules/, may rewrite
    // tsconfig.json via extends). A same-process `oam run` after `oam install`
    // must not see the stale "not found" / stale parsed paths. Drop the
    // current thread's default resolver caches so the next resolve re-stats
    // the filesystem and re-reads tsconfig.
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
    let cache_root = project_dir
        .join("node_modules")
        .join(".oam")
        .join("precompile");

    for (key, entry) in &to_install {
        let Some(resolved) = &entry.resolved else {
            // Entries without a resolved URL are link/file deps or the root;
            // skip silently.
            continue;
        };

        // The key is e.g. "node_modules/foo" or "node_modules/@scope/bar".
        // The extraction target is project_dir joined with the key.
        let dest = project_dir.join(key);
        // Extract the npm package name from the lockfile key.
        // Keys can be deeply nested ("node_modules/react/node_modules/scheduler"),
        // so take the portion after the LAST "node_modules/" segment.
        let pkg_name = key
            .rfind("node_modules/")
            .map(|pos| &key[pos + "node_modules/".len()..])
            .unwrap_or(key);

        // Skip already-installed packages (idempotency for partial installs).
        if dest.join("package.json").is_file() {
            installed += 1;
            // Still check scripts for already-installed packages so a trusted
            // package's scripts run (or an untrusted one warns) even on a warm
            // install.
            if let Some(ps) = check_lifecycle_scripts(&dest, pkg_name, &trust, &mut errors) {
                pending_scripts.push(ps);
            }
            continue;
        }

        match fetch_and_extract(&rt, &client, resolved, entry.integrity.as_deref(), &dest) {
            Ok(()) => {
                installed += 1;
                if let Some(ps) = check_lifecycle_scripts(&dest, pkg_name, &trust, &mut errors) {
                    pending_scripts.push(ps);
                }
                if precompile
                    && let Err(e) =
                        crate::precompile::precompile_package(&dest, pkg_name, &cache_root)
                {
                    errors.push(Diagnostic::new(
                        "OAM-PKG0008",
                        Severity::Warning,
                        Origin::Install,
                        format!("precompile {pkg_name}: {e}"),
                    ));
                }
            }
            Err(e) => {
                errors.push(diag("OAM-PKG0004", format!("failed to install {key}: {e}")));
            }
        }
    }

    // Bin linking pass (runs even if some packages failed — link what we can).
    // Bin failures are non-fatal: a missing .bin shim doesn't invalidate the
    // install of the package itself, so the diagnostic lands on the Partial
    // path instead of poisoning an otherwise-clean install with Err.
    link_bins(&node_modules, &to_install, &mut errors);

    // A4: now that the tree and .bin shims are in place, run the lifecycle
    // scripts collected from trusted packages.
    run_lifecycle_scripts(&pending_scripts, &node_modules, &mut errors);

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

fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    // reqwest with rustls-no-provider needs an explicit provider install.
    // The ring provider is already compiled (workspace dep via rustls).
    let _ = rustls::crypto::ring::default_provider().install_default();

    reqwest::ClientBuilder::new()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(120))
        .user_agent(concat!("oam/", env!("CARGO_PKG_VERSION")))
        .build()
}

// ── Fetch + extract ─────────────────────────────────────────────────────

fn fetch_and_extract(
    rt: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    url: &str,
    integrity: Option<&str>,
    dest: &Path,
) -> Result<(), String> {
    // Retry up to 3 times: registry CDNs see transient 5xx and TLS resets on
    // CI runners; npm itself defaults to 5. One retry (the original) gave up
    // too easily for flaky-network environments without adding meaningful
    // resilience over zero retries.
    let data = download_with_retry(rt, client, url, 3)?;

    if let Some(sri) = integrity {
        verify_integrity(&data, sri)?;
    }

    extract_tarball(&data, dest)
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

pub(crate) fn verify_integrity(data: &[u8], integrity: &str) -> Result<(), String> {
    let (algo, hash_b64) = integrity
        .split_once('-')
        .ok_or_else(|| format!("invalid SRI format: {integrity}"))?;
    match algo {
        "sha512" => {
            use sha2::{Digest, Sha512};
            let computed = Sha512::digest(data);
            let expected =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, hash_b64)
                    .map_err(|e| format!("invalid base64 in integrity: {e}"))?;
            if computed.as_slice() != expected.as_slice() {
                return Err("sha512 integrity mismatch".into());
            }
        }
        "sha1" => {
            use sha1::Digest;
            let computed = sha1::Sha1::digest(data);
            let expected =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, hash_b64)
                    .map_err(|e| format!("invalid base64 in integrity: {e}"))?;
            if computed.as_slice() != expected.as_slice() {
                return Err("sha1 integrity mismatch".into());
            }
        }
        other => return Err(format!("unsupported hash algorithm: {other}")),
    }
    Ok(())
}

// ── Tarball extraction ──────────────────────────────────────────────────

fn extract_tarball(data: &[u8], dest: &Path) -> Result<(), String> {
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

    let result = extract_into(data, &tmp);
    match result {
        Ok(()) => {
            // A peer that FULLY installed dest (package.json present) between
            // the caller's idempotency check and now wins; drop our tempdir
            // and accept the peer's copy -- both are SRI-verified upstream,
            // so functionally equivalent. We key on package.json presence,
            // NOT bare existence: dest may exist as a stale/partial dir from
            // a prior crashed extract (dir created, package.json never
            // written), which must be replaced, not accepted.
            if dest.join("package.json").is_file() {
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
                    if dest.join("package.json").is_file() {
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

fn extract_into(data: &[u8], dest: &Path) -> Result<(), String> {
    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);

    // Clean the dest once: `Path::join` followed by lexical `starts_with`
    // would falsely reject a non-canonical `dest` (e.g. `/tmp/./x` joined
    // with a normal file). Cleaning both sides puts the comparison on a
    // canonical basis.
    let dest_clean = path_clean(dest);

    for entry in archive.entries().map_err(|e| format!("tar entries: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let path = entry.path().map_err(|e| format!("tar entry path: {e}"))?;

        // npm tarballs wrap everything under a `package/` prefix.
        let rel = path.strip_prefix("package").unwrap_or(&path).to_path_buf();

        if rel.as_os_str().is_empty() {
            continue;
        }

        let target = dest.join(&rel);
        let target_clean = path_clean(&target);

        // Safety: reject paths that escape the destination. The previous
        // `target.starts_with(dest)` check was purely lexical and missed
        // `..` segments inside a relative entry -- e.g. an entry
        // `package/../../escape.txt` produced `dest/../../escape.txt`, which
        // lexically starts with `dest` but resolves outside it. Cleaning
        // the target collapses `..` so the prefix check actually works.
        if !target_clean.starts_with(&dest_clean) {
            return Err(format!(
                "tarball path traversal: {} escapes {}",
                rel.display(),
                dest.display()
            ));
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }

        // Handle directories and files.
        let kind = entry.header().entry_type();
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
            create_symlink_entry(&link_name, &target, &dest_clean, &rel)?;
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
/// skipped with a warning rather than created or aborted.
///
/// Unix creates the symlink. On non-Unix it is skipped with a warning: Windows
/// symlinks need a privilege normal accounts lack and require knowing file-vs-
/// dir up front, and npm itself shims bins separately rather than relying on
/// tarball symlinks there. Either way the warning means the omission is not
/// silent (the prior MVP dropped symlinks with no trace).
fn create_symlink_entry(
    link_name: &Path,
    target: &Path,
    dest_clean: &Path,
    rel: &Path,
) -> Result<(), String> {
    let link_parent = target.parent().unwrap_or(target);
    let resolved = path_clean(&link_parent.join(link_name));
    if !resolved.starts_with(dest_clean) {
        eprintln!(
            "oam install: warning: skipping symlink {} -> {} (target outside package)",
            rel.display(),
            link_name.display()
        );
        return Ok(());
    }

    #[cfg(unix)]
    {
        // Replace any stale entry so a re-extract is idempotent.
        let _ = std::fs::remove_file(target);
        std::os::unix::fs::symlink(link_name, target).map_err(|e| {
            format!(
                "symlink {} -> {}: {e}",
                target.display(),
                link_name.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        eprintln!(
            "oam install: warning: skipping symlink {} -> {} (symlinks need a privilege on this platform)",
            rel.display(),
            link_name.display()
        );
    }
    Ok(())
}

// ── Bin linking ─────────────────────────────────────────────────────────

/// Scan installed packages for `bin` entries and create shims in
/// `node_modules/.bin/`.
///
/// `errors` is appended to (never returned as `Err`) so the caller can
/// classify once. The create-dir and canonicalize-failure paths surface
/// `OAM-PKG0005` warnings; per-shim failures are non-diagnostic (logged to
/// stderr) because a missing shim is recoverable on the next install.
pub(crate) fn link_bins(
    node_modules: &Path,
    packages: &[(&str, &LockfileEntry)],
    errors: &mut Vec<Diagnostic>,
) {
    let bin_dir = node_modules.join(".bin");

    // Collect bin entries.
    let mut bins: Vec<(String, std::path::PathBuf)> = Vec::new();

    for (key, entry) in packages {
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

        // Package name for the default bin name (last segment of key).
        let pkg_name = key.rsplit('/').next().unwrap_or(key);

        match &bin_field {
            serde_json::Value::String(s) => {
                bins.push((pkg_name.to_string(), pkg_dir.join(s)));
            }
            serde_json::Value::Object(map) => {
                for (name, path) in map {
                    if let Some(s) = path.as_str() {
                        bins.push((name.clone(), pkg_dir.join(s)));
                    }
                }
            }
            _ => {}
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
        if let Err(e) = create_bin_shim(&bin_dir, &bin_dir_canonical, name, target, node_modules) {
            // Non-fatal: warn and continue.
            eprintln!("oam install: warning: bin shim for {name}: {e}");
        }
    }
}

fn create_bin_shim(
    bin_dir: &Path,
    bin_dir_canonical: &Path,
    name: &str,
    target: &Path,
    _node_modules: &Path,
) -> Result<(), String> {
    // Compute relative path from .bin to the target.
    let rel = pathdiff(target, bin_dir_canonical);

    #[cfg(windows)]
    {
        // Windows: create a .cmd shim.
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
}

/// After a package is extracted, inspect its `package.json` for lifecycle
/// scripts. Returns `Some(PendingScripts)` ONLY when the package is in the
/// trust list and has at least one lifecycle script (the caller runs them in a
/// later pass). An untrusted package with scripts emits an `OAM-PKG0007`
/// warning and returns `None` -- scripts run only after explicit
/// `oam trust add <pkg>`. No scripts, or unreadable package.json, returns
/// `None` quietly.
fn check_lifecycle_scripts(
    dest: &Path,
    pkg_name: &str,
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

    if !trust.is_trusted(pkg_name) {
        let version = json.get("version").and_then(|v| v.as_str()).unwrap_or("?");
        let script_list = present
            .iter()
            .map(|(p, _)| *p)
            .collect::<Vec<_>>()
            .join(", ");
        errors.push(Diagnostic::new(
            "OAM-PKG0007",
            Severity::Warning,
            Origin::Install,
            format!(
                "{pkg_name}@{version} has lifecycle script(s) ({script_list}) -- skipped; run 'oam trust add {pkg_name}' to allow"
            ),
        ));
        return None;
    }

    Some(PendingScripts {
        pkg_dir: dest.to_path_buf(),
        pkg_name: pkg_name.to_string(),
        scripts: present,
    })
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
        for (phase, cmd) in &pkg.scripts {
            if let Err(e) = run_one_script(cmd, phase, &pkg.pkg_dir, &bin_dir) {
                errors.push(Diagnostic::new(
                    "OAM-PKG0009",
                    Severity::Error,
                    Origin::Install,
                    format!("{}: {phase} script failed: {e}", pkg.pkg_name),
                ));
            }
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

/// Run a single lifecycle script via the platform shell (`sh -c` / `cmd /C`),
/// with `node_modules/.bin` prepended to PATH and `npm_lifecycle_event` set.
fn run_one_script(cmd: &str, phase: &str, pkg_dir: &Path, bin_dir: &Path) -> Result<(), String> {
    // Prepend node_modules/.bin to PATH so scripts can call installed bins.
    let mut paths: Vec<PathBuf> = vec![bin_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let new_path = std::env::join_paths(paths).map_err(|e| format!("PATH join: {e}"))?;

    #[cfg(windows)]
    let (sh, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (sh, flag) = ("sh", "-c");

    let status = std::process::Command::new(sh)
        .arg(flag)
        .arg(cmd)
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
    // NOTE on coverage: the existing safety check at `extract_tarball`
    // (`target.starts_with(dest)`) is purely lexical and therefore cannot
    // detect `..`-based traversal — `dest.join("../escape.txt")` lexically
    // starts with `dest`. The check DOES catch absolute paths (where
    // `Path::join` replaces the base), and that's what this test exercises.
    // The `..` case is a known gap and is separately tracked.
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

        // Untrusted: warns, returns None.
        let empty = crate::trust::TrustConfig::default();
        let mut errors = Vec::new();
        assert!(check_lifecycle_scripts(&pkg, "p", &empty, &mut errors).is_none());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "OAM-PKG0007");

        // Trusted: returns the lifecycle scripts only (not "build"), no warning.
        let mut trusted = crate::trust::TrustConfig::default();
        trusted.add("p");
        let mut errors2 = Vec::new();
        let pending = check_lifecycle_scripts(&pkg, "p", &trusted, &mut errors2)
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
        let pending = vec![PendingScripts {
            pkg_dir: pkg_dir.clone(),
            pkg_name: "trusted-pkg".to_string(),
            scripts: vec![("postinstall", "echo done > marker.txt".to_string())],
        }];
        let mut errors = Vec::new();
        run_lifecycle_scripts(&pending, &nm, &mut errors);

        assert!(errors.is_empty(), "script should succeed; got {errors:?}");
        assert!(
            pkg_dir.join("marker.txt").is_file(),
            "postinstall should have created the marker"
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
            dev: None,
            optional: None,
            bin: Some(serde_json::json!({
                "tsc": "./bin/tsc",
                "tsserver": "./bin/tsserver"
            })),
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
            dev: None,
            optional: None,
            bin: Some(serde_json::json!(["cmd1", "cmd2"])),
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
}
