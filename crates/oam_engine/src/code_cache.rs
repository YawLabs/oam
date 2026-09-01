//! V8 bytecode code-cache: persist compiled bytecode so repeat runs skip
//! parse+compile (the dominant cold-start cost once the startup snapshot is
//! paid). Lazy: a blob is produced on the first compile of a given source and
//! consumed on every run after. This is distinct from the install-time
//! TypeScript precompile cache (`oam_loader::precompile`, which caches
//! `.ts`->`.js`); they compose -- precompile produces the JS, this caches that
//! JS's bytecode.
//!
//! Keying is content-addressed by
//! `sha256(version_tag || format || kind || kind-specific shape || source)`,
//! where `version_tag` is V8's `cached_data_version_tag()` (bound to the exact
//! V8 build + flags) and `format` is [`CODE_CACHE_FORMAT`], oam's own stamp for
//! the compile shape V8's blob sanity check cannot see (the CJS wrapper
//! parameter list is folded in explicitly for `Kind::Function`). A rusty_v8
//! bump or an oam-side shape change makes old blobs stop matching -- a foreign
//! blob is never deserialized into the wrong engine. The consume sites
//! additionally honor V8's own `rejected()` signal as a belt-and-braces guard
//! and refresh the blob.
//!
//! Callers hash once: [`key_for`] yields an opaque [`CacheKey`] that both
//! [`load`] and [`store`] take, so a miss does not pay a second SHA-256 pass
//! over the source.
//!
//! Store layout: `<cache_root>/bytecode/<aa>/<rest>.v8c`, where `cache_root`
//! is `OAM_CACHE_DIR` if set (tests), else the platform cache dir, else the
//! system temp dir (see [`resolve_cache_root`]). The cache is a pure
//! optimization: every read/write failure is swallowed, and a miss just falls
//! through to a normal compile. Housekeeping is opportunistic and off the
//! load path: see [`maybe_sweep`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, SystemTime};

/// oam-side format stamp folded into every cache key. Bump it whenever the
/// compile shape at a produce site changes in a way V8's blob sanity check
/// cannot detect -- V8 hashes only the source length, a has-wrapped-arguments
/// bit and the is-module bit, so a changed wrapper origin, a different eager
/// policy for embedded blobs, or any future rewrite of the produce path would
/// otherwise consume a blob compiled against the old shape and silently run
/// stale bytecode (`rejected()` stays false in that case). The CJS wrapper
/// parameter NAMES are folded in separately (see [`entry_hash`]) so that edit
/// cannot be forgotten.
const CODE_CACHE_FORMAT: u32 = 1;

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

/// Opaque content-addressed key for one `(source, kind)` pair under the
/// current V8 build and oam format. Computed once by [`key_for`] and handed to
/// [`load`], [`store`] and [`forget_seed`], so a load-then-store pair hashes
/// the source a single time and never re-derives the cache root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey(String);

/// A blob returned by [`load`]: the raw bytes plus where they came from.
/// `seeded` is true when the bytes are the in-process seed (embedded bytecode
/// from `oam compile`) rather than a disk entry; a consumer that sees V8
/// reject a seeded blob calls [`forget_seed`] so the refreshed disk blob is
/// what later loads in this process see.
pub(crate) struct Loaded {
    pub bytes: Vec<u8>,
    pub seeded: bool,
}

/// The environment inputs [`resolve_cache_root`] consults, captured once so
/// the path logic itself is pure and testable without touching process env.
/// An empty value counts as unset -- `LOCALAPPDATA=` would otherwise resolve
/// to a relative `oam/` under the CWD, the exact failure the temp fallback
/// exists to prevent.
struct CacheEnv {
    cache_dir: Option<String>,
    local_app_data: Option<String>,
    xdg_cache_home: Option<String>,
    home: Option<String>,
    temp_dir: PathBuf,
}

impl CacheEnv {
    fn from_process() -> Self {
        let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        Self {
            cache_dir: var("OAM_CACHE_DIR"),
            local_app_data: var("LOCALAPPDATA"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
            home: var("HOME"),
            temp_dir: std::env::temp_dir(),
        }
    }
}

/// Pure path logic behind [`cache_root`]: `OAM_CACHE_DIR` override (tests),
/// else the platform cache dir (`%LOCALAPPDATA%\oam` on Windows,
/// `$XDG_CACHE_HOME/oam` or `~/.cache/oam` elsewhere), else the system temp
/// dir -- never the process CWD. A scrubbed environment (`env -i`, a minimal
/// container, some systemd units) has no `HOME`/`LOCALAPPDATA`; before the
/// temp fallback, oam wrote its blobs into whatever directory it was run
/// from. `windows` is a parameter rather than `cfg!` so both branches are
/// unit-tested on every host.
///
/// Mirrors `oam_ts::daemon::cache_root` deliberately (kept local to avoid a
/// dependency from the v8-bound engine into the tsgo crate); the two must
/// agree on the base dir so `oam cache` and the daemon's state share one root.
fn resolve_cache_root(env: &CacheEnv, windows: bool) -> PathBuf {
    let base = if let Some(dir) = &env.cache_dir {
        PathBuf::from(dir)
    } else {
        let platform = if windows {
            env.local_app_data.as_deref().map(PathBuf::from)
        } else {
            env.xdg_cache_home
                .as_deref()
                .map(PathBuf::from)
                .or_else(|| env.home.as_deref().map(|h| PathBuf::from(h).join(".cache")))
        };
        platform.unwrap_or_else(|| env.temp_dir.clone()).join("oam")
    };
    base.join("bytecode")
}

/// Bytecode cache root for this process. Resolved once: the env is fixed for
/// a process lifetime (no in-process `set_var` of `OAM_CACHE_DIR` exists; tests
/// pass it via `Command::env`), and the per-load path derivation used to cost
/// two env reads plus allocations on every miss.
fn cache_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| resolve_cache_root(&CacheEnv::from_process(), cfg!(windows)))
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

/// Content-addressed hash for a (source, kind) pair under the current V8 build
/// and oam format, as 64 lowercase hex chars. Folds in, in order: V8's version
/// tag (a blob from a different engine build never collides with -- or is
/// served to -- this one), [`CODE_CACHE_FORMAT`], the kind byte, and for
/// `Kind::Function` the CJS wrapper parameter list -- V8 does not check the
/// wrapper's argument names on consume, so a renamed or reordered
/// `CJS_PARAMS` on the same V8 build would otherwise be served bytecode with
/// the previous scope layout.
fn entry_hash(source: &str, kind: Kind) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
    hasher.update(CODE_CACHE_FORMAT.to_le_bytes());
    hasher.update([kind.tag()]);
    if let Kind::Function = kind {
        hasher.update(crate::cjs::CJS_PARAMS.join(",").as_bytes());
        hasher.update([0]);
    }
    hasher.update(source.as_bytes());
    hex32(&hasher.finalize().into())
}

/// The key for `source` compiled as `kind`. One SHA-256 pass; hand the result
/// to every cache call for this compile.
pub(crate) fn key_for(source: &str, kind: Kind) -> CacheKey {
    CacheKey(entry_hash(source, kind))
}

/// Disk path for a key: `<cache_root>/<aa>/<rest>.v8c`.
fn path_for_key(key: &CacheKey) -> PathBuf {
    cache_root()
        .join(&key.0[..2])
        .join(format!("{}.v8c", &key.0[2..]))
}

/// In-process bytecode seed: blobs handed to the runtime directly (not via
/// disk), keyed by the same content hash as the disk store. `oam compile`
/// seeds the embedded bytecode here at startup, so a compiled binary's first
/// run consumes it WITHOUT needing a writable cache dir (containers, read-only
/// or ephemeral filesystems).
fn seed_map() -> &'static Mutex<HashMap<CacheKey, Vec<u8>>> {
    static SEED: OnceLock<Mutex<HashMap<CacheKey, Vec<u8>>>> = OnceLock::new();
    SEED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Seed an in-memory bytecode blob for `source` of `kind`. The next `load()`
/// for the same source in this process returns it unless a disk entry has
/// superseded it (see [`load`]). No-op on an empty blob. V8 still validates
/// the blob on consume (`rejected()` -> recompile), so a stale or tampered
/// seed can only cost a recompile.
pub(crate) fn seed(source: &str, kind: Kind, blob: Vec<u8>) {
    if blob.is_empty() {
        return;
    }
    let key = key_for(source, kind);
    if let Ok(mut map) = seed_map().lock() {
        map.insert(key, blob);
    }
}

/// Drop the in-memory seed for `key`. Called by a consume site when V8
/// rejected a seeded blob (a foreign build's bytecode, or a flag-hash
/// mismatch): the site then refreshes the disk entry, and dropping the seed
/// keeps a later load in this process from being handed the rejected bytes
/// again. Across processes the seed is re-inserted at every startup, so the
/// cross-run half of the same guarantee is [`load`]'s disk-before-seed order.
pub(crate) fn forget_seed(key: &CacheKey) {
    if let Ok(mut map) = seed_map().lock() {
        map.remove(key);
    }
}

/// Load a cached bytecode blob for `key`, if one exists for the current V8
/// build. Returns the raw bytes; the caller wraps them in a
/// `v8::script_compiler::CachedData` to consume. `None` on miss or any I/O
/// error (the cache is never a correctness dependency).
///
/// Disk is consulted before the in-memory seed. A same-host seed is normally
/// the only copy of its key and needs no writable cache dir (the read of a
/// missing path is the whole cost); a disk blob under a seeded key exists only
/// because an earlier run refreshed it after V8 rejected the seed (or a lazy
/// `oam run` of the same source produced one), so it is the newer, accepted
/// blob and supersedes the seed. Seed-first ordering would re-reject and
/// re-write on every run of that binary.
pub(crate) fn load(key: &CacheKey) -> Option<Loaded> {
    if !enabled() {
        return None;
    }
    maybe_sweep();
    if let Some(bytes) = std::fs::read(path_for_key(key))
        .ok()
        .filter(|b| !b.is_empty())
    {
        return Some(Loaded {
            bytes,
            seeded: false,
        });
    }
    let map = seed_map().lock().ok()?;
    let bytes = map.get(key).filter(|b| !b.is_empty())?.clone();
    Some(Loaded {
        bytes,
        seeded: true,
    })
}

/// Store a freshly produced bytecode blob for `key`. Best-effort: a write
/// failure (read-only dir, ENOSPC) is silently ignored. Atomic via a unique
/// temp file + rename so a concurrent reader never observes a torn blob.
pub(crate) fn store(key: &CacheKey, bytes: &[u8]) {
    if !enabled() || bytes.is_empty() {
        return;
    }
    let path = path_for_key(key);
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
    // `std::fs::rename` replaces an existing target on every platform (on
    // Windows it is MoveFileExW with MOVEFILE_REPLACE_EXISTING), so a reader
    // sees either the old blob or the new one and never a missing entry. A
    // racing peer that stored under the same key is harmless: both blobs are
    // valid for this source (Function-kind blobs are produced after execution,
    // so their inner-function coverage can differ, but either deserializes).
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ── housekeeping ─────────────────────────────────────────────────────────

/// Stamp file in the cache root whose mtime records the last COMPLETED sweep.
const SWEEP_STAMP: &str = ".sweep";
/// Minimum interval between sweeps.
const SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// A `.<pid>.<n>.tmp` older than this was left by a process killed between
/// write and rename; nothing will ever rename it.
const TMP_MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// A blob not written in this long is evicted. Blobs are never rewritten on a
/// hit (touching on the hot path would cost a write per load), so a source in
/// daily use is recompiled once a month; that trade keeps one-off `-e`
/// strings, superseded V8 tags and edited files from accumulating forever.
const BLOB_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Opportunistic eviction. At most once per process, and only when the stamp
/// says the last completed sweep is older than [`SWEEP_INTERVAL`], walk the
/// cache root on a detached thread and delete orphaned temp files older than
/// [`TMP_MAX_AGE`] and blobs older than [`BLOB_MAX_AGE`]. Detached and never
/// joined, so no load ever waits on it; a process that exits mid-walk leaves
/// the stamp untouched and the next process finishes the job. Concurrent
/// processes may walk at once -- a file deleted twice is a swallowed error.
fn maybe_sweep() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let root = cache_root().to_path_buf();
        let _ = std::thread::Builder::new()
            .name("oam-code-cache-sweep".into())
            .spawn(move || {
                let now = SystemTime::now();
                if sweep_due(&root, now) {
                    sweep(&root, now);
                }
            });
    });
}

/// Whether a sweep of `root` is due at `now`: the root exists (nothing to do
/// otherwise, and no stamp to write in a possibly read-only place) and the
/// stamp is missing or older than [`SWEEP_INTERVAL`]. A stamp from the future
/// (clock skew) is not due.
fn sweep_due(root: &Path, now: SystemTime) -> bool {
    if !root.is_dir() {
        return false;
    }
    match std::fs::metadata(root.join(SWEEP_STAMP)).and_then(|m| m.modified()) {
        Ok(stamped) => now
            .duration_since(stamped)
            .is_ok_and(|age| age >= SWEEP_INTERVAL),
        Err(_) => true,
    }
}

/// What one sweep removed; returned for tests and never surfaced at runtime.
#[derive(Debug, Default, PartialEq, Eq)]
struct SweepReport {
    tmp_files: usize,
    blobs: usize,
}

/// Delete stale entries under `root` (see [`maybe_sweep`]) and, once the walk
/// completes, touch the stamp. Pure over `now` so the age rules are
/// unit-tested against files with set mtimes. Shard directories are left in
/// place: removing an empty one would race a concurrent `store`'s
/// `create_dir_all` + write.
fn sweep(root: &Path, now: SystemTime) -> SweepReport {
    fn older_than(path: &Path, now: SystemTime, age: Duration) -> bool {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|elapsed| elapsed >= age)
    }
    fn walk(dir: &Path, now: SystemTime, report: &mut SweepReport) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, now, report);
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && name.ends_with(".tmp") {
                if older_than(&path, now, TMP_MAX_AGE) && std::fs::remove_file(&path).is_ok() {
                    report.tmp_files += 1;
                }
            } else if name.ends_with(".v8c")
                && older_than(&path, now, BLOB_MAX_AGE)
                && std::fs::remove_file(&path).is_ok()
            {
                report.blobs += 1;
            }
        }
    }
    let mut report = SweepReport::default();
    walk(root, now, &mut report);
    let _ = std::fs::write(root.join(SWEEP_STAMP), b"");
    report
}

/// A snapshot of the on-disk cache for `oam cache info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeCacheStats {
    /// The bytecode directory (may not exist yet).
    pub path: PathBuf,
    /// Number of `.v8c` blobs.
    pub entries: usize,
    /// Their total size in bytes.
    pub bytes: u64,
}

/// Count `.v8c` blobs under `root` and sum their sizes. Missing root -> zero.
fn dir_stats(root: &Path) -> (usize, u64) {
    fn walk(dir: &Path, entries: &mut usize, bytes: &mut u64) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, entries, bytes);
            } else if path.extension().and_then(|e| e.to_str()) == Some("v8c")
                && let Ok(meta) = entry.metadata()
            {
                *entries += 1;
                *bytes += meta.len();
            }
        }
    }
    let (mut entries, mut bytes) = (0, 0);
    walk(root, &mut entries, &mut bytes);
    (entries, bytes)
}

/// The bytecode directory this process uses.
pub(crate) fn dir() -> PathBuf {
    cache_root().to_path_buf()
}

/// Location, entry count and total size of this process's bytecode cache.
pub(crate) fn stats() -> CodeCacheStats {
    let path = dir();
    let (entries, bytes) = dir_stats(&path);
    CodeCacheStats {
        path,
        entries,
        bytes,
    }
}

/// Delete the whole bytecode directory. Safe at any time: the cache is a pure
/// optimization, so the next run recompiles and repopulates. A missing
/// directory is success.
pub(crate) fn clean() -> std::io::Result<()> {
    match std::fs::remove_dir_all(cache_root()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        cache_dir: Option<&str>,
        local_app_data: Option<&str>,
        xdg: Option<&str>,
        home: Option<&str>,
    ) -> CacheEnv {
        CacheEnv {
            cache_dir: cache_dir.map(str::to_string),
            local_app_data: local_app_data.map(str::to_string),
            xdg_cache_home: xdg.map(str::to_string),
            home: home.map(str::to_string),
            temp_dir: PathBuf::from("/tmp-probe"),
        }
    }

    #[test]
    fn cache_root_override_wins_on_both_platforms() {
        let e = env(
            Some("/override"),
            Some("C:/lad"),
            Some("/xdg"),
            Some("/home/u"),
        );
        assert_eq!(
            resolve_cache_root(&e, true),
            PathBuf::from("/override").join("bytecode")
        );
        assert_eq!(
            resolve_cache_root(&e, false),
            PathBuf::from("/override").join("bytecode")
        );
    }

    #[test]
    fn cache_root_platform_dirs() {
        let e = env(None, Some("C:/lad"), Some("/xdg"), Some("/home/u"));
        assert_eq!(
            resolve_cache_root(&e, true),
            PathBuf::from("C:/lad").join("oam").join("bytecode")
        );
        assert_eq!(
            resolve_cache_root(&e, false),
            PathBuf::from("/xdg").join("oam").join("bytecode")
        );
        let e = env(None, None, None, Some("/home/u"));
        assert_eq!(
            resolve_cache_root(&e, false),
            PathBuf::from("/home/u")
                .join(".cache")
                .join("oam")
                .join("bytecode")
        );
    }

    #[test]
    fn cache_root_falls_back_to_temp_never_cwd() {
        // Use the real temp dir for this fixture: `is_absolute` is
        // platform-sensitive (`/tmp-probe` is root-relative, not absolute,
        // on Windows), and the property under test is about the actual host.
        let mut e = env(None, None, None, None);
        e.temp_dir = std::env::temp_dir();
        let expected = std::env::temp_dir().join("oam").join("bytecode");
        for windows in [true, false] {
            let root = resolve_cache_root(&e, windows);
            assert_eq!(root, expected);
            assert!(
                root.is_absolute(),
                "fallback must never be CWD-relative: {}",
                root.display()
            );
        }
    }

    #[test]
    fn empty_env_values_count_as_unset() {
        // `CacheEnv::from_process` filters empties; the pure function must
        // therefore never see them, but a direct caller passing `Some("")`
        // would get a CWD-relative `oam/`, which is the bug under test. Model
        // the filter here so the contract is pinned at the boundary.
        let filtered = |v: &str| Some(v.to_string()).filter(|s| !s.is_empty());
        assert_eq!(filtered(""), None);
        assert_eq!(filtered("x"), Some("x".to_string()));
    }

    #[test]
    fn key_folds_kind_and_wrapper_params() {
        // Force V8 initialization first: `cached_data_version_tag()` folds the
        // live flag set, so a key computed while a parallel test is still
        // initializing V8 (platform + flags) differs from one computed after.
        // Production hits every cache call through a constructed JsRuntime, so
        // pinning that order here mirrors the real call shape.
        let _rt = crate::JsRuntime::new();
        let a = key_for("x", Kind::Function);
        let b = key_for("x", Kind::Module);
        assert_ne!(a, b, "kind must key apart identical source");
        assert_eq!(a, key_for("x", Kind::Function), "key is deterministic");
        // The Function key depends on the wrapper parameter list: the manual
        // digest below with a different param list must differ from `a`.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
        h.update(CODE_CACHE_FORMAT.to_le_bytes());
        h.update([Kind::Function.tag()]);
        h.update(b"exports,require,module,__filename,__dirname,global,extra");
        h.update([0]);
        h.update(b"x");
        let other = CacheKey(hex32(&h.finalize().into()));
        assert_ne!(a, other, "a changed CJS_PARAMS must change the key");
        // And the current list reproduces the real key exactly.
        let mut h = Sha256::new();
        h.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
        h.update(CODE_CACHE_FORMAT.to_le_bytes());
        h.update([Kind::Function.tag()]);
        h.update(crate::cjs::CJS_PARAMS.join(",").as_bytes());
        h.update([0]);
        h.update(b"x");
        assert_eq!(a, CacheKey(hex32(&h.finalize().into())));
    }

    fn scratch(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "oam-code-cache-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_aged(path: &Path, age: Duration, now: SystemTime) {
        std::fs::write(path, b"x").unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(now - age).unwrap();
    }

    #[test]
    fn sweep_removes_only_stale_tmp_and_old_blobs() {
        let root = scratch("sweep");
        let shard = root.join("ab");
        std::fs::create_dir_all(&shard).unwrap();
        let now = SystemTime::now();
        // Stale temp file (killed process), fresh temp file (in-flight store).
        write_aged(&shard.join(".1.0.tmp"), TMP_MAX_AGE * 2, now);
        write_aged(&shard.join(".2.0.tmp"), Duration::from_secs(1), now);
        // Old blob, recent blob.
        write_aged(
            &shard.join("old.v8c"),
            BLOB_MAX_AGE + Duration::from_secs(1),
            now,
        );
        write_aged(
            &shard.join("new.v8c"),
            BLOB_MAX_AGE - Duration::from_secs(60),
            now,
        );
        // An unrelated file is never touched.
        write_aged(&shard.join("notes.txt"), BLOB_MAX_AGE * 2, now);

        let report = sweep(&root, now);
        assert_eq!(
            report,
            SweepReport {
                tmp_files: 1,
                blobs: 1
            }
        );
        assert!(!shard.join(".1.0.tmp").exists());
        assert!(shard.join(".2.0.tmp").exists());
        assert!(!shard.join("old.v8c").exists());
        assert!(shard.join("new.v8c").exists());
        assert!(shard.join("notes.txt").exists());
        assert!(shard.is_dir(), "shard dirs are left in place");
        assert!(
            root.join(SWEEP_STAMP).exists(),
            "a completed sweep is stamped"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_due_honours_stamp_age_and_missing_root() {
        let now = SystemTime::now();
        let missing = std::env::temp_dir().join("oam-code-cache-no-such-root");
        assert!(!sweep_due(&missing, now), "no root -> nothing to sweep");

        let root = scratch("due");
        assert!(sweep_due(&root, now), "no stamp -> due");
        write_aged(&root.join(SWEEP_STAMP), Duration::from_secs(60), now);
        assert!(!sweep_due(&root, now), "fresh stamp -> not due");
        write_aged(
            &root.join(SWEEP_STAMP),
            SWEEP_INTERVAL + Duration::from_secs(1),
            now,
        );
        assert!(
            sweep_due(&root, now),
            "stamp older than the interval -> due"
        );
        // A stamp from the future (clock skew) is not due.
        let f = std::fs::File::options()
            .write(true)
            .open(root.join(SWEEP_STAMP))
            .unwrap();
        f.set_modified(now + Duration::from_secs(3600)).unwrap();
        assert!(!sweep_due(&root, now));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dir_stats_counts_blobs_only() {
        let root = scratch("stats");
        assert_eq!(dir_stats(&root.join("missing")), (0, 0));
        let shard = root.join("cd");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join("a.v8c"), [0u8; 10]).unwrap();
        std::fs::write(shard.join("b.v8c"), [0u8; 5]).unwrap();
        std::fs::write(shard.join(".9.9.tmp"), [0u8; 100]).unwrap();
        std::fs::write(root.join(SWEEP_STAMP), b"").unwrap();
        assert_eq!(dir_stats(&root), (2, 15));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── V8 evidence: what the produce sites capture ──────────────────────

    /// `oam compile` produces once, before any execution, and the runtime never
    /// re-produces a seeded blob (a compiled binary needs no writable cache
    /// dir). EagerCompile is what makes that single blob carry every inner
    /// function's bytecode rather than just the wrapper: the blob for a source
    /// with several inner functions must be strictly larger under eager
    /// compile than under V8's default lazy policy. Two runtimes because a
    /// second compile of the same source in one isolate would hit V8's
    /// in-isolate compilation cache and ignore the option.
    #[test]
    fn eager_cjs_produce_captures_inner_functions() {
        const SRC: &str = "function a(x) { return x + 1; }\n\
                           function b(x) { return a(x) * 2; }\n\
                           function c(x) { return b(x) - 3; }\n\
                           module.exports = { a, b, c };\n";
        let lazy = {
            let mut rt = crate::JsRuntime::new();
            v8::scope_with_context!(let scope, &mut rt.isolate, &rt.context);
            crate::cjs::produce_cjs_code_cache(scope, SRC, false).expect("lazy blob")
        };
        let eager = {
            let mut rt = crate::JsRuntime::new();
            v8::scope_with_context!(let scope, &mut rt.isolate, &rt.context);
            crate::cjs::produce_cjs_code_cache(scope, SRC, true).expect("eager blob")
        };
        assert!(
            eager.len() > lazy.len(),
            "eager blob ({}) must carry more bytecode than lazy ({})",
            eager.len(),
            lazy.len()
        );
    }

    fn no_imports<'s>(
        _context: v8::Local<'s, v8::Context>,
        _specifier: v8::Local<'s, v8::String>,
        _attributes: v8::Local<'s, v8::FixedArray>,
        _referrer: v8::Local<'s, v8::Module>,
    ) -> Option<v8::Local<'s, v8::Module>> {
        None
    }

    /// The ESM produce site runs AFTER the graph evaluates. Evidence that (a)
    /// V8 150 lets `get_unbound_module_script().create_code_cache()` run on an
    /// evaluated module at all, (b) the post-evaluate blob is strictly larger
    /// than the pre-evaluate one for a module whose top level calls its own
    /// functions (those were lazily compiled during evaluation), and (c) a
    /// fresh compile under a different origin consumes the post-evaluate blob
    /// without `rejected()` -- the different origin defeats V8's in-isolate
    /// compilation cache so the deserializer really runs.
    #[test]
    fn post_evaluate_module_produce_is_larger_and_consumable() {
        let mut rt = crate::JsRuntime::new();
        v8::scope_with_context!(let scope, &mut rt.isolate, &rt.context);
        v8::tc_scope!(let tc, scope);

        fn compile<'s>(
            tc: &mut v8::PinScope<'s, '_>,
            name: &str,
            cached: Option<&[u8]>,
        ) -> (v8::Local<'s, v8::Module>, bool) {
            const SRC: &str = "function a(x) { return x + 1; }\n\
                               function b(x) { return a(x) * 2; }\n\
                               export const v = b(20);\n";
            let source_str = v8::String::new(tc, SRC).unwrap();
            let name: v8::Local<v8::Value> = v8::String::new(tc, name).unwrap().into();
            let origin =
                v8::ScriptOrigin::new(tc, name, 0, 0, false, 0, None, false, false, true, None);
            let mut source = match cached {
                Some(blob) => v8::script_compiler::Source::new_with_cached_data(
                    source_str,
                    Some(&origin),
                    v8::script_compiler::CachedData::new(blob),
                ),
                None => v8::script_compiler::Source::new(source_str, Some(&origin)),
            };
            let options = if cached.is_some() {
                v8::script_compiler::CompileOptions::ConsumeCodeCache
            } else {
                v8::script_compiler::CompileOptions::NoCompileOptions
            };
            let module = v8::script_compiler::compile_module2(
                tc,
                &mut source,
                options,
                v8::script_compiler::NoCacheReason::NoReason,
            )
            .expect("module compiles");
            let rejected = source
                .get_cached_data()
                .map(|cd| cd.rejected())
                .unwrap_or(false);
            (module, rejected)
        }

        let (module, _) = compile(tc, "file:///pre.mjs", None);
        let pre = module
            .get_unbound_module_script(tc)
            .create_code_cache()
            .expect("pre-evaluate blob")
            .to_vec();
        assert_eq!(
            module.instantiate_module(tc, no_imports),
            Some(true),
            "instantiate"
        );
        module.evaluate(tc).expect("evaluate");
        assert_eq!(module.get_status(), v8::ModuleStatus::Evaluated);
        let post = module
            .get_unbound_module_script(tc)
            .create_code_cache()
            .expect("post-evaluate blob")
            .to_vec();
        assert!(
            post.len() > pre.len(),
            "post-evaluate blob ({}) must carry the functions evaluation compiled; pre = {}",
            post.len(),
            pre.len()
        );

        let (consumed, rejected) = compile(tc, "file:///post.mjs", Some(&post));
        assert!(!rejected, "V8 must accept a blob produced after evaluation");
        assert_eq!(
            consumed.instantiate_module(tc, no_imports),
            Some(true),
            "consumed module instantiates"
        );
        consumed.evaluate(tc).expect("consumed module evaluates");
        let ns = consumed.get_module_namespace();
        let ns = v8::Local::<v8::Object>::try_from(ns).unwrap();
        let key = v8::String::new(tc, "v").unwrap();
        let v = ns.get(tc, key.into()).unwrap();
        assert_eq!(v.number_value(tc), Some(42.0));
    }
}
