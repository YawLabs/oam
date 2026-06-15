//! Lockfile-driven package installer (MVP: frozen-lockfile mode only).
//!
//! Reads npm's `package-lock.json` v3, downloads tarballs from the resolved
//! URLs, verifies SRI integrity hashes, extracts into `node_modules`, and
//! creates `.bin` shims. Equivalent to `npm ci` for oam projects.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
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
struct LockfileEntry {
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

/// Summary returned on success.
#[derive(Debug)]
pub struct InstallSummary {
    pub packages_installed: usize,
    pub elapsed: Duration,
}

// ── Public entry point ──────────────────────────────────────────────────

/// Install packages from a lockfile.
///
/// `project_dir` is the directory containing `package-lock.json`.
/// `frozen` must be `true` for the MVP (the lockfile is authoritative; no
/// resolution or lockfile mutation).
pub fn install(project_dir: &Path, frozen: bool) -> Result<InstallSummary, Vec<Diagnostic>> {
    if !frozen {
        return Err(vec![diag(
            "OAM-PKG0006",
            "only --frozen-lockfile mode is supported in this release",
        )]);
    }
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

    for (key, entry) in &to_install {
        let Some(resolved) = &entry.resolved else {
            // Entries without a resolved URL are link/file deps or the root;
            // skip silently.
            continue;
        };

        // The key is e.g. "node_modules/foo" or "node_modules/@scope/bar".
        // The extraction target is project_dir joined with the key.
        let dest = project_dir.join(key);

        // Skip already-installed packages (idempotency for partial installs).
        if dest.join("package.json").is_file() {
            installed += 1;
            continue;
        }

        match fetch_and_extract(&rt, &client, resolved, entry.integrity.as_deref(), &dest) {
            Ok(()) => {
                installed += 1;
            }
            Err(e) => {
                errors.push(diag(
                    "OAM-PKG0004",
                    format!("failed to install {key}: {e}"),
                ));
            }
        }
    }

    // Bin linking pass (runs even if some packages failed — link what we can).
    if let Err(e) = link_bins(&node_modules, &to_install) {
        errors.push(e);
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(InstallSummary {
        packages_installed: installed,
        elapsed: started.elapsed(),
    })
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
    let data = download_with_retry(rt, client, url, 1)?;

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
        match rt.block_on(async { client.get(url).send().await }) {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    last_err = format!("HTTP {status} for {url}");
                    continue;
                }
                match rt.block_on(async { resp.bytes().await }) {
                    Ok(bytes) => return Ok(bytes.to_vec()),
                    Err(e) => {
                        last_err = format!("reading body from {url}: {e}");
                    }
                }
            }
            Err(e) => {
                last_err = format!("request to {url}: {e}");
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
            let expected = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                hash_b64,
            )
            .map_err(|e| format!("invalid base64 in integrity: {e}"))?;
            if computed.as_slice() != expected.as_slice() {
                return Err("sha512 integrity mismatch".into());
            }
        }
        "sha1" => {
            use sha1::Digest;
            let computed = sha1::Sha1::digest(data);
            let expected = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                hash_b64,
            )
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
    let gz = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries().map_err(|e| format!("tar entries: {e}"))? {
        let mut entry = entry.map_err(|e| format!("tar entry: {e}"))?;
        let path = entry.path().map_err(|e| format!("tar entry path: {e}"))?;

        // npm tarballs wrap everything under a `package/` prefix.
        let rel = path
            .strip_prefix("package")
            .unwrap_or(&path)
            .to_path_buf();

        if rel.as_os_str().is_empty() {
            continue;
        }

        let target = dest.join(&rel);

        // Safety: reject paths that escape the destination.
        if !target.starts_with(dest) {
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
        }
        // Symlinks and other entry types are skipped for MVP.
    }
    Ok(())
}

// ── Bin linking ─────────────────────────────────────────────────────────

/// Scan installed packages for `bin` entries and create shims in
/// `node_modules/.bin/`.
fn link_bins(
    node_modules: &Path,
    packages: &[(&str, &LockfileEntry)],
) -> Result<(), Diagnostic> {
    let bin_dir = node_modules.join(".bin");

    // Collect bin entries.
    let mut bins: Vec<(String, std::path::PathBuf)> = Vec::new();

    for (key, entry) in packages {
        let pkg_dir = node_modules
            .parent()
            .unwrap_or(node_modules)
            .join(key);

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
        return Ok(());
    }

    std::fs::create_dir_all(&bin_dir).map_err(|e| {
        Diagnostic::new(
            "OAM-PKG0005",
            Severity::Warning,
            Origin::Install,
            format!("could not create .bin directory: {e}"),
        )
    })?;

    for (name, target) in &bins {
        if let Err(e) = create_bin_shim(&bin_dir, name, target, node_modules) {
            // Non-fatal: warn and continue.
            eprintln!("oam install: warning: bin shim for {name}: {e}");
        }
    }

    Ok(())
}

fn create_bin_shim(
    bin_dir: &Path,
    name: &str,
    target: &Path,
    _node_modules: &Path,
) -> Result<(), String> {
    // Compute relative path from .bin to the target.
    let rel = pathdiff(target, bin_dir);

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
fn pathdiff(target: &Path, base: &Path) -> std::path::PathBuf {
    // Canonicalize what we can; fall back to the raw paths.
    let t = std::fs::canonicalize(target)
        .or_else(|_| std::path::absolute(target))
        .unwrap_or_else(|_| target.to_path_buf());
    let b = std::fs::canonicalize(base)
        .or_else(|_| std::path::absolute(base))
        .unwrap_or_else(|_| base.to_path_buf());

    let mut t_parts: Vec<_> = t.components().collect();
    let mut b_parts: Vec<_> = b.components().collect();

    // Drop common prefix.
    while !t_parts.is_empty()
        && !b_parts.is_empty()
        && t_parts[0] == b_parts[0]
    {
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

fn diag(code: &str, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(code, Severity::Error, Origin::Install, message)
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
        let b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            hash.as_slice(),
        );
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
        let b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            hash.as_slice(),
        );
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
            "oam-install-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let result = install(&tmp, true);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].code, "OAM-PKG0001");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_empty_deps_succeeds() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-install-test-empty-{}",
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
        let result = install(&tmp, true);
        assert!(result.is_ok());
        let summary = result.unwrap();
        assert_eq!(summary.packages_installed, 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn lockfile_version_2_rejected() {
        let tmp = std::env::temp_dir().join(format!(
            "oam-install-test-v2-{}",
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
        let result = install(&tmp, true);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert_eq!(errors[0].code, "OAM-PKG0002");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_tarball_strips_package_prefix() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
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
            "oam-extract-test-{}",
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
}
