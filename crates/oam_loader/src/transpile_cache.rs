//! Content-addressed transpile cache: persistent oxc output for EVERY
//! transpiled source (project files included -- the install-time precompile
//! cache covers only node_modules), so a warm run skips the oxc
//! parse/transform/codegen pipeline entirely and goes straight to the V8
//! bytecode-cache lookup.
//!
//! Layout: `<cache_root>/transpile/<aa>/<hash>.js`, where `hash` is the
//! SHA-256 of `transpile_fingerprint(path)` + a NUL + the source bytes.
//! Content addressing makes existence the freshness proof -- no sidecar, no
//! mtime -- and folding the fingerprint in means an oxc upgrade, a
//! `TRANSPILE_FORMAT_VERSION` bump, or a changed tsconfig JSX setting
//! simply misses instead of serving stale output.
//!
//! Artifact format: a `// oam-transpile v1 <sha256-of-body>` header line,
//! then the JS. The key hashes the INPUTS, so an entry cannot be validated
//! against its own name; the self-hash is what keeps a corrupted entry from
//! executing. It is verified on every read and the header stripped -- a
//! mismatch is a miss that re-transpiles, never wrong output served.
//!
//! Every filesystem failure, read or write, is a silent miss: the cache is
//! an accelerator, and the fallback (transpile again) is always correct.
//! `OAM_TRANSPILE_CACHE=0|off|false|no` disables it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

const ARTIFACT_HEADER: &str = "// oam-transpile v1 ";

/// Opt-out switch, read once per process (the cache is consulted on every
/// transpiled module load; a per-load getenv would be pure overhead).
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| !disabled_by(std::env::var("OAM_TRANSPILE_CACHE").ok().as_deref()))
}

fn disabled_by(value: Option<&str>) -> bool {
    matches!(value, Some("0" | "off" | "false" | "no"))
}

/// Cache root: `OAM_CACHE_DIR` override (tests), else the platform cache
/// dir, else the temp dir. A deliberate THIRD copy of this derivation --
/// `oam_engine::code_cache::cache_root` and `oam_ts::daemon::cache_root`
/// are the others -- kept local so the loader depends on neither the
/// v8-bound engine nor the tsgo crate. This copy adds the temp-dir fallback
/// so a host with no platform cache env still caches.
fn cache_root() -> PathBuf {
    let base = if let Ok(dir) = std::env::var("OAM_CACHE_DIR") {
        PathBuf::from(dir)
    } else {
        platform_cache_base().unwrap_or_else(|| std::env::temp_dir().join("oam"))
    };
    base.join("transpile")
}

#[cfg(windows)]
fn platform_cache_base() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|dir| PathBuf::from(dir).join("oam"))
}

#[cfg(not(windows))]
fn platform_cache_base() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join("oam"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".cache").join("oam"))
}

/// Lowercase hex of a 32-byte digest (no `hex` crate dependency).
fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex32(&hasher.finalize().into())
}

/// Content hash of everything that determines the transpiled output: the
/// full transpile fingerprint (oxc versions, format version, resolved
/// source type, resolved JSX settings) and the source text. The NUL
/// separator keeps fingerprint/source concatenations unambiguous (the
/// fingerprint never contains one).
///
/// NOTE: this is a different digest from the precompile cache's sidecar
/// (sha256 of the source alone, precompile.rs `source_hash`), so a load
/// that consults both caches hashes the source twice. The key shapes are
/// deliberately different -- path-addressed + sidecar there, content-
/// addressed here -- and unifying them belongs to the precompile side.
fn entry_key(path: &Path, source: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(crate::transpile_fingerprint(path).as_bytes());
    hasher.update([0u8]);
    hasher.update(source.as_bytes());
    hex32(&hasher.finalize().into())
}

/// `<root>/<aa>/<rest>.js` -- fan out on the first hash byte so one flat
/// dir never accumulates every artifact (mirrors the bytecode cache).
fn entry_path(root: &Path, key: &str) -> PathBuf {
    root.join(&key[..2]).join(format!("{}.js", &key[2..]))
}

/// Cached transpile output for (path, source), or None (miss, disabled, or
/// corrupt entry). A hit returns the verbatim `transpile_typescript` output
/// that `store` was given.
pub fn try_get(path: &Path, source: &str) -> Option<String> {
    if !enabled() {
        return None;
    }
    try_get_in(&cache_root(), path, source)
}

fn try_get_in(root: &Path, path: &Path, source: &str) -> Option<String> {
    let text = std::fs::read_to_string(entry_path(root, &entry_key(path, source))).ok()?;
    let rest = text.strip_prefix(ARTIFACT_HEADER)?;
    let (stored_hash, body) = rest.split_once('\n')?;
    (sha256_hex(body) == stored_hash.trim()).then(|| body.to_string())
}

/// Store transpiled output. Atomic within the cache dir: written to a
/// unique temp sibling, then renamed over the final name -- a concurrent
/// reader sees the old entry or the new one, never a torn file (and the
/// self-hash header catches torn writes on filesystems where rename is not
/// atomic). Best-effort: any failure just means the next run transpiles.
pub fn store(path: &Path, source: &str, js: &str) {
    if !enabled() {
        return;
    }
    store_in(&cache_root(), path, source, js);
}

fn store_in(root: &Path, path: &Path, source: &str, js: &str) {
    let final_path = entry_path(root, &entry_key(path, source));
    let Some(parent) = final_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".tmp-{}-{nanos}", std::process::id()));
    let payload = format!("{ARTIFACT_HEADER}{}\n{js}", sha256_hex(js));
    if std::fs::write(&tmp, payload).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, &final_path).is_err() {
        // Windows refuses rename-over-existing: a concurrent writer already
        // published this content-addressed entry, so ours is redundant.
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "oam-transpile-cache-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn store_then_get_roundtrips_verbatim() {
        let root = temp_root("roundtrip");
        let path = Path::new("app.ts");
        let source = "const n: number = 42;\n";
        let js = "const n = 42;\n";
        assert_eq!(try_get_in(&root, path, source), None, "cold cache misses");
        store_in(&root, path, source, js);
        assert_eq!(try_get_in(&root, path, source).as_deref(), Some(js));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupted_entry_is_a_miss_not_wrong_output() {
        let root = temp_root("corrupt");
        let path = Path::new("app.ts");
        let source = "const n: number = 1;\n";
        store_in(&root, path, source, "const n = 1;\n");
        let entry = entry_path(&root, &entry_key(path, source));
        // Tamper with the body without updating the self-hash header.
        let tampered = std::fs::read_to_string(&entry)
            .unwrap()
            .replace("const n = 1", "throw new Error('corrupt')");
        std::fs::write(&entry, tampered).unwrap();
        assert_eq!(try_get_in(&root, path, source), None);
        // An entry with no header at all is a miss too.
        std::fs::write(&entry, "const n = 1;\n").unwrap();
        assert_eq!(try_get_in(&root, path, source), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn key_tracks_source_and_fingerprint() {
        let source = "const n = 1;\n";
        let ts = entry_key(Path::new("a.ts"), source);
        assert_eq!(ts, entry_key(Path::new("a.ts"), source), "stable");
        assert_ne!(
            ts,
            entry_key(Path::new("a.ts"), "const n = 2;\n"),
            "source is in the key"
        );
        // .cts fingerprints as CommonJS, .ts as module -- different slots
        // even for identical text.
        assert_ne!(
            ts,
            entry_key(Path::new("a.cts"), source),
            "fingerprint is in the key"
        );
    }

    #[test]
    fn opt_out_values_parse() {
        for off in ["0", "off", "false", "no"] {
            assert!(disabled_by(Some(off)), "{off}");
        }
        for on in [
            None,
            Some("1"),
            Some("on"),
            Some("true"),
            Some("yes"),
            Some(""),
        ] {
            assert!(!disabled_by(on), "{on:?}");
        }
    }
}
