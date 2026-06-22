//! V8 bytecode code-cache: persist compiled bytecode so repeat runs skip
//! parse+compile (the dominant cold-start cost once the startup snapshot is
//! paid). Lazy: a blob is produced on the first compile of a given source and
//! consumed on every run after. This is distinct from the install-time
//! TypeScript precompile cache (`oam_loader::precompile`, which caches
//! `.ts`->`.js`); they compose -- precompile produces the JS, this caches that
//! JS's bytecode.
//!
//! Keying is content-addressed by `sha256(version_tag || kind || source)`,
//! where `version_tag` is V8's `cached_data_version_tag()` (bound to the exact
//! V8 build + flags). A rusty_v8 bump changes the tag, so stale blobs simply
//! stop matching -- a foreign blob is never deserialized into the wrong engine.
//! The consume site additionally honors V8's own `rejected()` signal as a
//! belt-and-braces guard and refreshes the blob.
//!
//! Store layout: `<cache_root>/bytecode/<aa>/<rest>.v8c`, where `cache_root`
//! is `OAM_CACHE_DIR` if set (tests), else the platform cache dir. The cache
//! is a pure optimization: every read/write failure is swallowed, and a miss
//! just falls through to a normal compile.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Whether the bytecode cache is active for this process. On by default; set
/// `OAM_CODE_CACHE=0` (or `off`/`false`/`no`) to disable it entirely -- no
/// consume, no produce, no write. Useful for benchmarking a cold compile,
/// debugging a suspected cache issue, or a read-only environment where the
/// write is unwanted. Read once: the env is fixed for a process lifetime, and
/// keeping it off the per-module-load hot path matters.
///
/// `pub(crate)` so the produce call sites can skip the `create_code_cache()`
/// serialize work too, not just the read/write -- a true off switch leaves no
/// residual cost.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("OAM_CODE_CACHE").as_deref(),
            Ok("0") | Ok("off") | Ok("false") | Ok("no")
        )
    })
}

/// What was compiled. ESM modules and CJS require-wrappers produce different
/// bytecode, so they are keyed apart even for identical source text.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    /// ESM module bytecode (the `compile_module` site).
    Module,
    /// CJS require-wrapper bytecode (the `compile_function` site).
    Function,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Kind::Module => b'm',
            Kind::Function => b'f',
        }
    }
}

/// Cache root: `OAM_CACHE_DIR` override (tests), else the platform cache dir.
/// Mirrors `oam_ts::daemon::cache_root` deliberately (kept local to avoid a
/// dependency from the v8-bound engine into the tsgo crate).
fn cache_root() -> PathBuf {
    let base = if let Ok(dir) = std::env::var("OAM_CACHE_DIR") {
        PathBuf::from(dir)
    } else {
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("oam")
        }
        #[cfg(not(windows))]
        {
            std::env::var("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                        .join(".cache")
                })
                .join("oam")
        }
    };
    base.join("bytecode")
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

/// Content-addressed hash for a (source, kind) pair under the current V8 build:
/// `sha256(version_tag || kind || source)` as 64 lowercase hex chars. The V8
/// version tag is folded in so a blob from a different engine build never
/// collides with -- or is served to -- this one.
fn entry_hash(source: &str, kind: Kind) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
    hasher.update([kind.tag()]);
    hasher.update(source.as_bytes());
    hex32(&hasher.finalize().into())
}

/// Disk path for a content hash: `<cache_root>/<aa>/<rest>.v8c`.
fn path_for_hash(hash: &str) -> PathBuf {
    cache_root()
        .join(&hash[..2])
        .join(format!("{}.v8c", &hash[2..]))
}

/// In-process bytecode seed: blobs handed to the runtime directly (not via
/// disk), keyed by the same content hash as the disk store. `oam compile`
/// seeds the embedded bytecode here at startup, so a compiled binary's first
/// run consumes it WITHOUT needing a writable cache dir (containers, read-only
/// or ephemeral filesystems). `load()` checks the seed before disk.
fn seed_map() -> &'static Mutex<HashMap<String, Vec<u8>>> {
    static SEED: OnceLock<Mutex<HashMap<String, Vec<u8>>>> = OnceLock::new();
    SEED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Seed an in-memory bytecode blob for `source` of `kind`. The next `load()`
/// for the same source in this process returns it ahead of any disk entry.
/// No-op on an empty blob. V8 still validates the blob on consume (`rejected()`
/// -> recompile), so a stale or tampered seed can only cost a recompile.
pub(crate) fn seed(source: &str, kind: Kind, blob: Vec<u8>) {
    if blob.is_empty() {
        return;
    }
    let hash = entry_hash(source, kind);
    if let Ok(mut map) = seed_map().lock() {
        map.insert(hash, blob);
    }
}

/// Load a cached bytecode blob for `source` of `kind`, if one exists for the
/// current V8 build. Checks the in-memory seed first (embedded bytecode from
/// `oam compile`), then disk. Returns the raw bytes; the caller wraps them in a
/// `v8::script_compiler::CachedData` to consume. `None` on miss or any I/O
/// error (the cache is never a correctness dependency).
pub(crate) fn load(source: &str, kind: Kind) -> Option<Vec<u8>> {
    if !enabled() {
        return None;
    }
    let hash = entry_hash(source, kind);
    // In-memory seed (oam compile embedded bytecode) wins over disk: it needs
    // no writable cache dir.
    if let Ok(map) = seed_map().lock()
        && let Some(blob) = map.get(&hash)
        && !blob.is_empty()
    {
        return Some(blob.clone());
    }
    std::fs::read(path_for_hash(&hash))
        .ok()
        .filter(|b| !b.is_empty())
}

/// Store a freshly produced bytecode blob for `source` of `kind`. Best-effort:
/// a write failure (read-only dir, ENOSPC) is silently ignored. Atomic via a
/// unique temp file + rename so a concurrent reader never observes a torn blob.
pub(crate) fn store(source: &str, kind: Kind, bytes: &[u8]) {
    if !enabled() || bytes.is_empty() {
        return;
    }
    let path = path_for_hash(&entry_hash(source, kind));
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        std::process::id(),
        CTR.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    // rename() won't overwrite a non-empty file on Windows; remove the old
    // entry first. A racing peer that wrote the same content-addressed path is
    // harmless -- the bytes are identical.
    let _ = std::fs::remove_file(&path);
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}
