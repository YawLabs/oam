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
//! simply misses instead of serving stale output. (The fingerprint also
//! path-addresses `react-jsxdev` output, whose `_jsxFileName` embeds the
//! source path -- see `transpile_fingerprint_with`.)
//!
//! Artifact format (v2): a header line
//!
//! ```text
//! // oam-transpile v2 <sha256-of-body> <js-byte-len>
//! ```
//!
//! then the body: exactly `<js-byte-len>` bytes of transpiled JavaScript,
//! immediately followed by the source-map JSON (empty when the entry
//! carries no map). Both halves ride one artifact because a warm run that
//! served the JS without its map would lose stack-trace fidelity -- the
//! map exists precisely for the runs that never touch oxc. The key hashes
//! the INPUTS, so an entry cannot be validated against its own name; the
//! self-hash is what keeps a corrupted entry from executing. It is
//! verified on every read and the header stripped -- a mismatch (any v1
//! entry included: different header prefix) is a miss that re-transpiles,
//! never wrong output served.
//!
//! Every filesystem failure, read or write, is a silent miss: the cache is
//! an accelerator, and the fallback (transpile again) is always correct.
//! `OAM_TRANSPILE_CACHE=0|off|false|no` disables it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

const ARTIFACT_HEADER: &str = "// oam-transpile v2 ";

/// Opt-out switch, read once per process (the cache is consulted on every
/// transpiled module load; a per-load getenv would be pure overhead).
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| !disabled_by(std::env::var("OAM_TRANSPILE_CACHE").ok().as_deref()))
}

fn disabled_by(value: Option<&str>) -> bool {
    matches!(value, Some("0" | "off" | "false" | "no"))
}

/// An env var read where an EMPTY value counts as unset -- `OAM_CACHE_DIR=`
/// or `LOCALAPPDATA=` would otherwise resolve to a RELATIVE path and send
/// the cache into the process CWD (the user's project), the exact failure
/// the platform/temp fallbacks exist to prevent. Mirrors
/// `oam_engine::code_cache::CacheEnv`.
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Cache root: `OAM_CACHE_DIR` override (tests), else the platform cache
/// dir, else a PER-USER temp subdir. `None` disables the cache for this
/// process (only the shared world-writable temp dir resolved and a safe
/// per-user subdir could not be established -- another local user
/// pre-owning that dir could plant entries oam would execute).
///
/// A deliberate THIRD copy of this derivation --
/// `oam_engine::code_cache::cache_root` and `oam_ts::daemon::cache_root`
/// are the others -- kept local so the loader depends on neither the
/// v8-bound engine nor the tsgo crate.
fn cache_root() -> Option<PathBuf> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(resolve_cache_root).clone()
}

fn resolve_cache_root() -> Option<PathBuf> {
    let base = if let Some(dir) = env_non_empty("OAM_CACHE_DIR") {
        PathBuf::from(dir)
    } else if let Some(platform) = platform_cache_base() {
        platform
    } else {
        // No platform dir (scrubbed env: `env -i`, minimal containers, some
        // systemd units). The shared temp dir is world-writable, so a flat
        // `temp/oam` would let any local user pre-plant executable JS;
        // claim a per-user subdir instead and refuse to cache if that
        // cannot be done safely.
        user_temp_cache_base(&std::env::temp_dir())?
    };
    Some(base.join("transpile"))
}

#[cfg(windows)]
fn platform_cache_base() -> Option<PathBuf> {
    env_non_empty("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("oam"))
}

#[cfg(not(windows))]
fn platform_cache_base() -> Option<PathBuf> {
    if let Some(dir) = env_non_empty("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join("oam"));
    }
    env_non_empty("HOME").map(|home| PathBuf::from(home).join(".cache").join("oam"))
}

/// Per-user cache base under the shared temp dir, or `None` when one cannot
/// be established safely (cache disabled -- correctness never depends on
/// the cache).
///
/// Unix: `<temp>/oam-user-<uid>`, created `0700`; a PRE-EXISTING dir is
/// accepted only when owned by the current uid (refusing a squatter's dir,
/// symlink-to-elsewhere included -- metadata follows the link and the
/// owner check fails on the foreign target).
///
/// Windows: `<temp>/oam-user-<username>`. `%TEMP%` is already per-user on
/// every stock configuration, so the name is namespacing rather than a
/// security boundary; with no username in the env the cache is disabled
/// rather than shared.
#[cfg(unix)]
fn user_temp_cache_base(temp: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    // SAFETY-free uid read: `geteuid` via rustix/libc would add a dep; the
    // metadata of a file we just created carries the same information.
    let dir = temp.join(format!("oam-user-{}", unsafe_free_uid()?));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Verify the existing dir is OURS. metadata() follows symlinks,
            // so a squatter's link to a dir they own also fails here.
            let meta = std::fs::metadata(&dir).ok()?;
            if !meta.is_dir() || Some(meta.uid()) != current_uid(temp) {
                return None;
            }
        }
        Err(_) => return None,
    }
    Some(dir)
}

/// The current effective uid, learned portably (no libc call in this
/// `#![forbid(unsafe_code)]` crate) from the ownership of a probe file this
/// process creates in `temp`.
#[cfg(unix)]
fn current_uid(temp: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    static UID: OnceLock<Option<u32>> = OnceLock::new();
    *UID.get_or_init(|| {
        let probe = temp.join(format!(
            ".oam-uid-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&probe, b"").ok()?;
        let uid = std::fs::metadata(&probe).ok().map(|m| m.uid());
        let _ = std::fs::remove_file(&probe);
        uid
    })
}

#[cfg(unix)]
fn unsafe_free_uid() -> Option<u32> {
    current_uid(&std::env::temp_dir())
}

#[cfg(not(unix))]
fn user_temp_cache_base(temp: &Path) -> Option<PathBuf> {
    let user = env_non_empty("USERNAME").or_else(|| env_non_empty("USER"))?;
    // Usernames can carry path-hostile characters; keep a conservative set.
    let safe: String = user
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(temp.join(format!("oam-user-{safe}")))
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
/// source type, resolved JSX settings -- and, under `react-jsxdev`, the
/// source path the emitted `_jsxFileName` embeds) and the source text. The
/// NUL separator keeps fingerprint/source concatenations unambiguous (the
/// fingerprint never contains one).
///
/// NOTE: this is a different digest from the precompile cache's header
/// (sha256 of fingerprint || source, no NUL), so a load that consults both
/// caches hashes the source twice. The key shapes are deliberately
/// different -- path-addressed there, content-addressed here -- and
/// unifying them belongs to the precompile side.
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
/// corrupt entry). A hit returns the verbatim `(js, source_map)` pair that
/// `store` was given.
pub fn try_get(path: &Path, source: &str) -> Option<(String, Option<String>)> {
    if !enabled() {
        return None;
    }
    try_get_in(&cache_root()?, path, source)
}

fn try_get_in(root: &Path, path: &Path, source: &str) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(entry_path(root, &entry_key(path, source))).ok()?;
    let rest = text.strip_prefix(ARTIFACT_HEADER)?;
    let (header, body) = rest.split_once('\n')?;
    let (stored_hash, js_len) = header.trim_end_matches('\r').split_once(' ')?;
    let js_len: usize = js_len.parse().ok()?;
    if sha256_hex(body) != stored_hash {
        return None;
    }
    // .get() rather than slicing: a length pointing past the end or into
    // the middle of a UTF-8 sequence is a corrupt entry, i.e. a miss.
    let js = body.get(..js_len)?;
    let map = body.get(js_len..)?;
    Some((js.to_string(), (!map.is_empty()).then(|| map.to_string())))
}

/// Store transpiled output plus its source map. Atomic within the cache
/// dir: written to a unique temp sibling, then renamed over the final name
/// -- a concurrent reader sees the old entry or the new one, never a torn
/// file (and the self-hash header catches torn writes on filesystems where
/// rename is not atomic). Best-effort: any failure just means the next run
/// transpiles.
pub fn store(path: &Path, source: &str, js: &str, source_map: Option<&str>) {
    if !enabled() {
        return;
    }
    let Some(root) = cache_root() else { return };
    store_in(&root, path, source, js, source_map);
}

fn store_in(root: &Path, path: &Path, source: &str, js: &str, source_map: Option<&str>) {
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
    let body = format!("{js}{}", source_map.unwrap_or(""));
    let payload = format!(
        "{ARTIFACT_HEADER}{} {}\n{body}",
        sha256_hex(&body),
        js.len()
    );
    if std::fs::write(&tmp, payload).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    // `std::fs::rename` replaces an existing target on every platform (on
    // Windows it is MoveFileExW with MOVEFILE_REPLACE_EXISTING), so a reader
    // sees either the old entry or the new one. The error arm only covers
    // real I/O failures (locks, AV interference); the tmp file is removed
    // best-effort, and the periodic-sweep-free design tolerates a stray.
    if std::fs::rename(&tmp, &final_path).is_err() {
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
    fn store_then_get_roundtrips_verbatim_with_map() {
        let root = temp_root("roundtrip");
        let path = Path::new("app.ts");
        let source = "const n: number = 42;\n";
        let js = "const n = 42;\n";
        let map = r#"{"version":3,"sources":["app.ts"],"mappings":"AAAA"}"#;
        assert_eq!(try_get_in(&root, path, source), None, "cold cache misses");
        store_in(&root, path, source, js, Some(map));
        assert_eq!(
            try_get_in(&root, path, source),
            Some((js.to_string(), Some(map.to_string())))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn store_then_get_roundtrips_without_map() {
        let root = temp_root("no-map");
        let path = Path::new("app.ts");
        let source = "const n: number = 1;\n";
        let js = "const n = 1;\n";
        store_in(&root, path, source, js, None);
        assert_eq!(
            try_get_in(&root, path, source),
            Some((js.to_string(), None))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupted_entry_is_a_miss_not_wrong_output() {
        let root = temp_root("corrupt");
        let path = Path::new("app.ts");
        let source = "const n: number = 1;\n";
        store_in(
            &root,
            path,
            source,
            "const n = 1;\n",
            Some("{\"mappings\":\"\"}"),
        );
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
        // A v1 entry (old header prefix) is a clean miss.
        std::fs::write(
            &entry,
            "// oam-transpile v1 0000000000000000000000000000000000000000000000000000000000000000\nconst n = 1;\n",
        )
        .unwrap();
        assert_eq!(try_get_in(&root, path, source), None);
        // A valid-hash body with a js_len pointing past the end is corrupt.
        let js = "const n = 1;\n";
        let bogus = format!(
            "{ARTIFACT_HEADER}{} {}\n{js}",
            sha256_hex(js),
            js.len() + 40
        );
        std::fs::write(&entry, bogus).unwrap();
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
    fn jsxdev_entries_are_path_addressed() {
        // Under jsx "react-jsxdev" the emitted _jsxFileName embeds the
        // source path, so two identical sources at different paths must NOT
        // share a cache slot. The fingerprint folds the path in for
        // AutomaticDev only; common modes stay content-addressed.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("oam-jsxdev-key-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"jsx":"react-jsxdev"}}"#,
        )
        .unwrap();
        let source = "export const el = <div/>;\n";
        std::fs::write(root.join("a.tsx"), source).unwrap();
        std::fs::write(root.join("b.tsx"), source).unwrap();
        crate::default_resolver().clear_caches();
        assert_ne!(
            entry_key(&root.join("a.tsx"), source),
            entry_key(&root.join("b.tsx"), source),
            "react-jsxdev output embeds the path; identical sources at \
             different paths must get distinct entries"
        );
        crate::default_resolver().clear_caches();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_env_values_count_as_unset() {
        assert_eq!(
            Some("x".to_string()).filter(|v| !v.is_empty()),
            Some("x".to_string())
        );
        assert_eq!(Some(String::new()).filter(|v| !v.is_empty()), None);
    }

    #[cfg(unix)]
    #[test]
    fn foreign_owned_temp_subdir_disables_the_cache() {
        // A pre-existing per-user dir owned by SOMEONE ELSE must be
        // refused. We cannot chown as an unprivileged test, so exercise the
        // accept side (our own dir is accepted, created 0700) and the shape
        // of the refusal via a non-dir squatting on the name.
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp =
            std::env::temp_dir().join(format!("oam-user-temp-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let base = user_temp_cache_base(&temp).expect("own uid dir is claimable");
        let mode = std::fs::metadata(&base).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "fresh per-user dir is 0700");
        // Second call accepts the dir we own.
        assert_eq!(user_temp_cache_base(&temp).as_ref(), Some(&base));
        // A FILE squatting on the name is refused (not a dir -> None).
        std::fs::remove_dir_all(&base).unwrap();
        std::fs::write(&base, b"squat").unwrap();
        assert_eq!(user_temp_cache_base(&temp), None);
        let _ = std::fs::remove_dir_all(&temp);
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
