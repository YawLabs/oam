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

use std::path::PathBuf;

/// What was compiled. ESM modules and CJS require-wrappers produce different
/// bytecode, so they are keyed apart even for identical source text.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    /// ESM module bytecode. Wired by B3b (the `compile_module` site); the
    /// variant exists now so the cache key space is reserved and stable.
    #[allow(dead_code)]
    Module,
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

/// Content-addressed path for a (source, kind) pair under the current V8 build.
/// The V8 version tag is folded into the key so blobs from a different engine
/// build never collide with -- or get served to -- this one.
fn entry_path(source: &str, kind: Kind) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
    hasher.update([kind.tag()]);
    hasher.update(source.as_bytes());
    let hex = hex32(&hasher.finalize().into());
    cache_root().join(&hex[..2]).join(format!("{}.v8c", &hex[2..]))
}

/// Load a cached bytecode blob for `source` of `kind`, if one exists for the
/// current V8 build. Returns the raw bytes; the caller wraps them in a
/// `v8::script_compiler::CachedData` to consume. `None` on miss or any I/O
/// error (the cache is never a correctness dependency).
pub(crate) fn load(source: &str, kind: Kind) -> Option<Vec<u8>> {
    std::fs::read(entry_path(source, kind))
        .ok()
        .filter(|b| !b.is_empty())
}

/// Store a freshly produced bytecode blob for `source` of `kind`. Best-effort:
/// a write failure (read-only dir, ENOSPC) is silently ignored. Atomic via a
/// unique temp file + rename so a concurrent reader never observes a torn blob.
pub(crate) fn store(source: &str, kind: Kind, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let path = entry_path(source, kind);
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
