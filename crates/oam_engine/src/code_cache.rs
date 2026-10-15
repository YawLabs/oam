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
//! system temp dir (see [`resolve_cache_root`]). node's compile-cache API
//! moves it: `NODE_COMPILE_CACHE=<dir>` or `module.enableCompileCache(dir)`
//! puts the same `<aa>/<rest>.v8c` tree in `<dir>/oam-v<ver>-<arch>-<tag>`
//! (see [`version_dir_name`]). The cache stays ON by default either way --
//! node's is opt-in, oam's predates the API and costs nothing measurable on
//! a first run -- and `NODE_DISABLE_COMPILE_CACHE` turns it off like
//! `OAM_CODE_CACHE=0`. A sibling `<rest>.seedrej`
//! tombstone records that V8 rejected the embedded SEED for that key, which
//! is what lets a refreshed disk blob supersede the seed without giving the
//! user-writable cache dir priority over a compiled binary's embedded
//! bytecode (see [`load`]). The cache is a pure
//! optimization: every read/write failure is swallowed, and a miss just falls
//! through to a normal compile. Housekeeping is opportunistic and off the
//! load path: see [`maybe_sweep`].

use oam_diagnostics::own_files::{self, Audience};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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
    disabled_by().is_none()
}

/// The variable that turned the cache off, if one did: node's
/// `NODE_DISABLE_COMPILE_CACHE`, or oam's own `OAM_CODE_CACHE` switch.
/// `module.enableCompileCache()` names it in its DISABLED message ("Disabled
/// by NODE_DISABLE_COMPILE_CACHE", node's exact wording). Read once, like
/// every other cache input.
pub(crate) fn disabled_by() -> Option<&'static str> {
    static WHY: OnceLock<Option<&'static str>> = OnceLock::new();
    *WHY.get_or_init(|| {
        disabled_reason(
            std::env::var_os("NODE_DISABLE_COMPILE_CACHE").is_some(),
            std::env::var("OAM_CODE_CACHE").ok().as_deref(),
        )
    })
}

/// Pure half of [`disabled_by`]. node tests `NODE_DISABLE_COMPILE_CACHE` for
/// PRESENCE, not truthiness -- v22.22.2 reports DISABLED for `=1` and `=0`
/// alike (measured) -- so a set variable disables whatever its value. It is
/// checked first because a program that asks node's API why the cache is off
/// should be told about node's variable when both are set.
fn disabled_reason(node_disable: bool, oam_code_cache: Option<&str>) -> Option<&'static str> {
    if node_disable {
        Some("NODE_DISABLE_COMPILE_CACHE")
    } else if matches!(oam_code_cache, Some("0" | "off" | "false" | "no")) {
        Some("OAM_CODE_CACHE")
    } else {
        None
    }
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
/// current V8 build and oam format, plus the cache root its lookup used.
/// Computed once by [`key_for`] and handed to [`load`], [`store`] and
/// [`forget_seed`], so a load-then-store pair hashes the source a single
/// time and never re-derives the cache root.
///
/// The root rides in the key because the root can move between a lookup and
/// its store: `module.enableCompileCache(dir)` runs from inside a module
/// body, after the entry (and an ESM entry's whole static graph) has been
/// looked up but before any of them is stored -- ESM blobs are produced
/// after evaluation, a CJS blob after its body returns. Re-reading the root
/// at store time filed those entries under the new root while every later
/// run looked them up under the old one, so the bin that follows node's
/// advice (call `enableCompileCache` at its top) recompiled and rewrote its
/// own entry on every run. A store now lands where its lookup looked.
///
/// Equality and hashing use the content hash alone: the in-memory seed map
/// is keyed by content, and a seed must match whichever root the lookup
/// that finds it used.
#[derive(Clone, Debug)]
pub(crate) struct CacheKey {
    hash: String,
    root: Arc<Path>,
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl Eq for CacheKey {}

impl std::hash::Hash for CacheKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

/// A blob returned by [`load`]: the raw bytes plus where they came from.
/// `seeded` is true when the bytes are the in-process seed (embedded bytecode
/// from `oam compile`) rather than a disk entry; a consumer that sees V8
/// reject a seeded blob calls [`record_seed_rejection`] so the refreshed disk
/// blob is what later loads -- in this process and in later runs -- see.
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
    /// `NODE_COMPILE_CACHE`, made absolute against the start-up working
    /// directory (node resolves a relative value the same way; measured).
    node_compile_cache: Option<PathBuf>,
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
            node_compile_cache: var("NODE_COMPILE_CACHE")
                .map(|dir| std::path::absolute(&dir).unwrap_or_else(|_| PathBuf::from(dir))),
            cache_dir: var("OAM_CACHE_DIR"),
            local_app_data: var("LOCALAPPDATA"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
            home: var("HOME"),
            temp_dir: std::env::temp_dir(),
        }
    }
}

/// Pure path logic behind [`env_root`]: node's `NODE_COMPILE_CACHE`, as
/// `<dir>/<version_dir>` (see [`version_dir_name`]), else the
/// `OAM_CACHE_DIR` override (tests), else the platform cache dir (`%LOCALAPPDATA%\oam` on Windows,
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
///
/// `NODE_COMPILE_CACHE` outranks `OAM_CACHE_DIR` because it is the narrower
/// request: it names where compiled code goes, where `OAM_CACHE_DIR` moves
/// all of oam's caches (and is what a test harness sets to isolate a run, so
/// a program that names its own compile-cache dir must still get it).
fn resolve_cache_root(env: &CacheEnv, windows: bool, version_dir: &str) -> PathBuf {
    if let Some(dir) = &env.node_compile_cache {
        return dir.join(version_dir);
    }
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

/// The root the environment selects. Resolved once: the env is fixed for a
/// process lifetime (no in-process `set_var` of `OAM_CACHE_DIR` exists; tests
/// pass it via `Command::env`), and the per-load path derivation used to cost
/// two env reads plus allocations on every miss.
fn env_root() -> &'static Path {
    &env_selection().0
}

/// [`env_root`] as the shared handle a [`CacheKey`] carries, built once so
/// a key costs a refcount bump, not a path copy.
fn env_root_arc() -> Arc<Path> {
    static ROOT: OnceLock<Arc<Path>> = OnceLock::new();
    Arc::clone(ROOT.get_or_init(|| Arc::from(env_root())))
}

/// [`env_root`], and whether `NODE_COMPILE_CACHE` chose it. node creates the
/// `NODE_COMPILE_CACHE` directory at start-up and, when it cannot (the path
/// runs through a file, a read-only parent), runs with its compile cache off:
/// `getCompileCacheDir()` is undefined and a later `enableCompileCache()`
/// answers ENABLED (measured on v22.22.2). oam does the same create here and,
/// on failure, keeps its default root instead -- the cache stays on, but
/// node's API reports what node's would.
fn env_selection() -> &'static (PathBuf, bool) {
    static ROOT: OnceLock<(PathBuf, bool)> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut env = CacheEnv::from_process();
        let version_dir = version_dir_name();
        // A disabled cache creates nothing, as node's does: the DISABLED
        // answer still asks for the default root, and must not mkdir.
        if env.node_compile_cache.is_some() && enabled() {
            let root = resolve_cache_root(&env, cfg!(windows), &version_dir);
            if std::fs::create_dir_all(&root).is_ok() {
                return (root, true);
            }
            env.node_compile_cache = None;
        }
        (resolve_cache_root(&env, cfg!(windows), &version_dir), false)
    })
}

/// The root `module.enableCompileCache(dir)` chose for one isolate, kept in
/// that isolate's slot. Per isolate, not per process, because node's
/// compile-cache state is per Environment (measured on v22.22.2): a
/// `worker_threads` Worker sees `getCompileCacheDir()` undefined after the
/// main thread enabled the cache, can enable a directory of its own, and its
/// call never moves the main thread's cache. A process-wide root let a
/// worker's call redirect the main thread's lookups and stores mid-run.
/// Set at most once per isolate: every later `enableCompileCache` call
/// answers ALREADY_ENABLED with the first directory, as node's does.
struct ApiRoot(Arc<Path>);

/// The bytecode cache root for code compiled in `isolate`: the directory its
/// `module.enableCompileCache` chose, else the environment's. Read on the
/// isolate thread at lookup time and carried in the [`CacheKey`] from there
/// (see its doc); graph-prefetch workers get the value their isolate read.
pub(crate) fn root_for(isolate: &v8::Isolate) -> Arc<Path> {
    match isolate.get_slot::<ApiRoot>() {
        Some(ApiRoot(root)) => Arc::clone(root),
        None => env_root_arc(),
    }
}

/// oam's version, folded into every key and into [`version_dir_name`].
const OAM_VERSION: &str = env!("CARGO_PKG_VERSION");

/// V8's `cached_data_version_tag()`: a pure function of the V8 build and its
/// flags, both fixed once init has run. Read through V8 exactly once instead
/// of paying an FFI call per key (keys are also derived on graph-prefetch
/// workers, and one cached read keeps the off-isolate-thread V8 surface
/// minimal).
fn version_tag() -> u32 {
    static VERSION_TAG: OnceLock<u32> = OnceLock::new();
    *VERSION_TAG.get_or_init(v8::script_compiler::cached_data_version_tag)
}

/// The per-build subdirectory oam keeps inside a directory the user names
/// (`NODE_COMPILE_CACHE`, `module.enableCompileCache(dir)`):
/// `oam-v<version>-<arch>-<v8 tag>`. node does the same with
/// `v22.22.2-arm64-<hash>`; the `oam-` prefix keeps the two apart when node
/// and oam share one directory -- their V8 builds differ, so neither could
/// use the other's entries, and a shared name would mix the two formats in
/// one folder.
fn version_dir_name() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("oam-v{OAM_VERSION}-{arch}-{:08x}", version_tag())
}

/// What `module.getCompileCacheDir()` reports: the directory node's API
/// considers enabled -- by an earlier `enableCompileCache` or by
/// `NODE_COMPILE_CACHE` -- or `None`. oam's default cache (on without either)
/// is not reported: node's API answers for node's opt-in cache, and a program
/// that branches on `getCompileCacheDir() === undefined` to decide whether to
/// enable one must see the same answer it sees under node.
pub(crate) fn api_dir(isolate: &v8::Isolate) -> Option<PathBuf> {
    if !enabled() {
        return None;
    }
    if let Some(ApiRoot(root)) = isolate.get_slot::<ApiRoot>() {
        return Some(root.to_path_buf());
    }
    let (root, node_selected) = env_selection();
    node_selected.then(|| root.clone())
}

/// What `module.enableCompileCache()` uses with no directory: the root oam
/// already caches into, so enabling node's API does not move a warm cache.
pub(crate) fn default_dir() -> PathBuf {
    env_root().to_path_buf()
}

/// The subdirectory name `enableCompileCache(dir)` creates inside `dir`.
pub(crate) fn api_version_dir() -> String {
    version_dir_name()
}

/// Make `root` the cache root for code `isolate` compiles from now on, as
/// `enableCompileCache` does. Returns false when this isolate already chose
/// one; the caller then reports ALREADY_ENABLED with [`api_dir`]. Keys
/// already handed out keep the root their lookup used (see [`CacheKey`]).
pub(crate) fn enable_at(isolate: &mut v8::Isolate, root: PathBuf) -> bool {
    if isolate.get_slot::<ApiRoot>().is_some() {
        return false;
    }
    isolate.set_slot(ApiRoot(Arc::from(root)));
    true
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
/// served to -- this one), [`CODE_CACHE_FORMAT`], oam's version, the kind
/// byte, and for
/// `Kind::Function` the CJS wrapper parameter list -- V8 does not check the
/// wrapper's argument names on consume, so a renamed or reordered
/// `CJS_PARAMS` on the same V8 build would otherwise be served bytecode with
/// the previous scope layout.
fn entry_hash(source: &str, kind: Kind) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(version_tag().to_le_bytes());
    hasher.update(CODE_CACHE_FORMAT.to_le_bytes());
    // oam's own version, NUL-terminated so it cannot run into the kind byte.
    // The V8 tag cannot see an oam release that changes what a produce site
    // compiles without a CODE_CACHE_FORMAT bump; keying on the release makes
    // a forgotten bump cost one recompile per upgrade instead of stale
    // bytecode.
    hasher.update(OAM_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update([kind.tag()]);
    if let Kind::Function = kind {
        hasher.update(crate::cjs::CJS_PARAMS.join(",").as_bytes());
        hasher.update([0]);
    }
    hasher.update(source.as_bytes());
    hex32(&hasher.finalize().into())
}

/// The key for `source` compiled as `kind`, looked up and stored under
/// `root` (normally [`root_for`] the compiling isolate). One SHA-256 pass;
/// hand the result to every cache call for this compile.
pub(crate) fn key_for(source: &str, kind: Kind, root: Arc<Path>) -> CacheKey {
    CacheKey {
        hash: entry_hash(source, kind),
        root,
    }
}

/// Disk path for a key: `<key root>/<aa>/<rest>.v8c`.
fn path_for_key(key: &CacheKey) -> PathBuf {
    key.root
        .join(&key.hash[..2])
        .join(format!("{}.v8c", &key.hash[2..]))
}

/// Disk path for a key's seed-rejection tombstone:
/// `<key root>/<aa>/<rest>.seedrej`. See [`record_seed_rejection`].
fn tombstone_path(key: &CacheKey) -> PathBuf {
    key.root
        .join(&key.hash[..2])
        .join(format!("{}.seedrej", &key.hash[2..]))
}

/// Read the disk blob for a key. `None` on miss, any I/O error, or an empty
/// file (never worth consuming).
fn read_blob(key: &CacheKey) -> Option<Vec<u8>> {
    std::fs::read(path_for_key(key))
        .ok()
        .filter(|b| !b.is_empty())
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
    // The root is irrelevant here: seeds match on content (see CacheKey).
    let key = key_for(source, kind, env_root_arc());
    if let Ok(mut map) = seed_map().lock() {
        map.insert(key, blob);
    }
}

/// Drop the in-memory seed for `key`. Called by a consume site when V8
/// rejected a seeded blob (a foreign build's bytecode, or a flag-hash
/// mismatch): the site then refreshes the disk entry, and dropping the seed
/// keeps a later load in this process from being handed the rejected bytes
/// again. Across processes the seed is re-inserted at every startup, so the
/// cross-run half of the same guarantee is the `.seedrej` tombstone
/// [`record_seed_rejection`] writes, which flips [`load`] to the refreshed
/// disk blob for that key.
pub(crate) fn forget_seed(key: &CacheKey) {
    if let Ok(mut map) = seed_map().lock() {
        map.remove(key);
    }
}

/// Record that V8 rejected the SEEDED blob for `key`: drop the in-memory seed
/// (a later load in this process must not see it again) and write a
/// `<key>.seedrej` tombstone next to the refreshed blob so later PROCESSES
/// prefer the disk entry over the embedded seed for this one key. Only the
/// rejection path writes it -- a compiled binary whose seed is accepted keeps
/// its zero-writes property. Best-effort, like every write here.
pub(crate) fn record_seed_rejection(key: &CacheKey) {
    forget_seed(key);
    let path = tombstone_path(key);
    if let Some(parent) = path.parent() {
        let _ = own_files::create_dir_all(parent, Audience::Kept);
    }
    let _ = own_files::write(&path, b"", Audience::Kept);
}

/// Load a cached bytecode blob for `key`, if one exists for the current V8
/// build. Returns the raw bytes; the caller wraps them in a
/// `v8::script_compiler::CachedData` to consume. `None` on miss or any I/O
/// error (the cache is never a correctness dependency).
///
/// The in-memory seed is consulted before disk: the embedded blob ships
/// INSIDE the binary, so it is the trusted copy -- disk-first would let any
/// V8-header-sane blob planted in the user-writable cache dir decide what a
/// compiled binary runs. The one case disk supersedes a seeded key is a
/// recorded rejection (the `.seedrej` tombstone [`record_seed_rejection`]
/// writes): the disk blob is then the newer, ACCEPTED refresh from an
/// earlier run, and staying seed-first there would re-reject and re-write on
/// every run of that binary.
pub(crate) fn load(key: &CacheKey) -> Option<Loaded> {
    if !enabled() {
        return None;
    }
    maybe_sweep(&key.root);
    let seeded = seed_map()
        .lock()
        .ok()
        .and_then(|map| map.get(key).filter(|b| !b.is_empty()).cloned());
    if let Some(bytes) = seeded {
        if tombstone_path(key).is_file()
            && let Some(disk) = read_blob(key)
        {
            return Some(Loaded {
                bytes: disk,
                seeded: false,
            });
        }
        return Some(Loaded {
            bytes,
            seeded: true,
        });
    }
    read_blob(key).map(|bytes| Loaded {
        bytes,
        seeded: false,
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
    // The cache's own modes, not the running program's mask: `own_files`.
    if own_files::create_dir_all(parent, Audience::Kept).is_err() {
        return;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        std::process::id(),
        CTR.fetch_add(1, Ordering::Relaxed)
    ));
    if own_files::write(&tmp, bytes, Audience::Kept).is_err() {
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

/// Stamp file in the cache root whose mtime records the last STARTED sweep.
/// Written BEFORE the walk, doubling as a writability probe -- see [`sweep`].
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
/// joined, so no load ever waits on it; a process that exits mid-walk has
/// already stamped, so the leftover work waits for the next interval rather
/// than the next process. Concurrent
/// processes may walk at once -- a file deleted twice is a swallowed error.
fn maybe_sweep(root: &Path) {
    // One gate per root, not per process: an isolate's
    // `enableCompileCache(dir)` points its keys at a new root after the
    // environment's may have been swept, and a directory a program names
    // needs the same housekeeping or it grows without bound. The gate and
    // the swept path are the same value, so no root is skipped or walked
    // twice when a root is chosen between the two.
    static SWEPT: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let first = SWEPT
        .get_or_init(Mutex::default)
        .lock()
        .map(|mut swept| swept.insert(root.to_path_buf()))
        .unwrap_or(false);
    if !first {
        return;
    }
    let root = root.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("oam-code-cache-sweep".into())
        .spawn(move || {
            let now = SystemTime::now();
            if sweep_due(&root, now) {
                sweep(&root, now);
            }
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
    seed_tombstones: usize,
}

/// Delete stale entries under `root` (see [`maybe_sweep`]). The stamp is
/// touched FIRST, doubling as a writability probe: on an unwritable root (a
/// container image baked with a warm, read-only cache -- the exact
/// environment the seed path serves) the walk would delete nothing, a
/// post-walk stamp write would fail, and every subsequent process would
/// re-walk the whole tree forever. The trade: a process killed mid-walk
/// re-sweeps after the NEXT interval instead of immediately, which the
/// interval semantics allow. Pure over `now` so the age rules are
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
            } else if name.ends_with(".seedrej")
                && older_than(&path, now, BLOB_MAX_AGE)
                && std::fs::remove_file(&path).is_ok()
            {
                // A tombstone outliving its blob only costs one re-rejection
                // and refresh on the next run of the binary that seeded it.
                report.seed_tombstones += 1;
            }
        }
    }
    let mut report = SweepReport::default();
    if !write_stamp(&root.join(SWEEP_STAMP), now) {
        return report;
    }
    walk(root, now, &mut report);
    report
}

/// (Re)write the sweep stamp at `path` and set its mtime to `now`
/// explicitly. Returns false when the stamp cannot be created (the
/// writability probe [`sweep`] relies on).
///
/// The explicit `set_modified` is the load-bearing part. Rust std emulates
/// create+truncate on Windows as `OPEN_ALWAYS` plus a manual truncate, so
/// rewriting an existing 0-byte stamp with 0 bytes never bumps NTFS's
/// LastWriteTime: the stamp froze at its first-ever write and every process
/// that loaded a module re-walked the whole cache. A `set_modified` failure
/// (a filesystem without settable times) is swallowed -- the worst case is a
/// sweep that runs more often than the interval, never one that is skipped.
fn write_stamp(path: &Path, now: SystemTime) -> bool {
    match own_files::create(path, Audience::Kept) {
        Ok(file) => {
            let _ = file.set_modified(now);
            true
        }
        Err(_) => false,
    }
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

/// The bytecode directory the environment selects: what `oam cache` reports
/// and cleans. A directory a program picks with `enableCompileCache(dir)` is
/// that program's to manage, as under node.
pub(crate) fn dir() -> PathBuf {
    env_root().to_path_buf()
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
    match std::fs::remove_dir_all(env_root()) {
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
            node_compile_cache: None,
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
            resolve_cache_root(&e, true, "SUB"),
            PathBuf::from("/override").join("bytecode")
        );
        assert_eq!(
            resolve_cache_root(&e, false, "SUB"),
            PathBuf::from("/override").join("bytecode")
        );
    }

    #[test]
    fn node_compile_cache_wins_and_gets_the_version_dir() {
        // NODE_COMPILE_CACHE names where compiled code goes; it outranks the
        // broader OAM_CACHE_DIR and every platform default, and the blobs go
        // in the per-build subdirectory, never the named directory itself.
        let mut e = env(
            Some("/override"),
            Some("C:/lad"),
            Some("/xdg"),
            Some("/home/u"),
        );
        e.node_compile_cache = Some(PathBuf::from("/ncc"));
        for windows in [true, false] {
            assert_eq!(
                resolve_cache_root(&e, windows, "SUB"),
                PathBuf::from("/ncc").join("SUB")
            );
        }
    }

    #[test]
    fn disabled_reason_matches_node_presence_rule() {
        // node: any NODE_DISABLE_COMPILE_CACHE value disables, "0" included.
        assert_eq!(
            disabled_reason(true, None),
            Some("NODE_DISABLE_COMPILE_CACHE")
        );
        assert_eq!(
            disabled_reason(true, Some("0")),
            Some("NODE_DISABLE_COMPILE_CACHE")
        );
        for off in ["0", "off", "false", "no"] {
            assert_eq!(disabled_reason(false, Some(off)), Some("OAM_CODE_CACHE"));
        }
        assert_eq!(disabled_reason(false, Some("1")), None);
        assert_eq!(disabled_reason(false, None), None);
    }

    #[test]
    fn version_dir_names_oam_arch_and_v8_tag() {
        let _rt = crate::JsRuntime::new();
        let name = version_dir_name();
        let tag = format!("{:08x}", v8::script_compiler::cached_data_version_tag());
        assert!(name.starts_with(&format!("oam-v{OAM_VERSION}-")), "{name}");
        assert!(name.ends_with(&format!("-{tag}")), "{name}");
    }

    #[test]
    fn cache_root_platform_dirs() {
        let e = env(None, Some("C:/lad"), Some("/xdg"), Some("/home/u"));
        assert_eq!(
            resolve_cache_root(&e, true, "SUB"),
            PathBuf::from("C:/lad").join("oam").join("bytecode")
        );
        assert_eq!(
            resolve_cache_root(&e, false, "SUB"),
            PathBuf::from("/xdg").join("oam").join("bytecode")
        );
        let e = env(None, None, None, Some("/home/u"));
        assert_eq!(
            resolve_cache_root(&e, false, "SUB"),
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
            let root = resolve_cache_root(&e, windows, "SUB");
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
        let a = key_for("x", Kind::Function, env_root_arc());
        let b = key_for("x", Kind::Module, env_root_arc());
        assert_ne!(a, b, "kind must key apart identical source");
        assert_eq!(
            a,
            key_for("x", Kind::Function, env_root_arc()),
            "key is deterministic"
        );
        // The Function key depends on the wrapper parameter list: the manual
        // digest below with a different param list must differ from `a`.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
        h.update(CODE_CACHE_FORMAT.to_le_bytes());
        h.update(OAM_VERSION.as_bytes());
        h.update([0]);
        h.update([Kind::Function.tag()]);
        h.update(b"exports,require,module,__filename,__dirname,global,extra");
        h.update([0]);
        h.update(b"x");
        let other = hex32(&h.finalize().into());
        assert_ne!(a.hash, other, "a changed CJS_PARAMS must change the key");
        // And the current list reproduces the real key exactly.
        let mut h = Sha256::new();
        h.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
        h.update(CODE_CACHE_FORMAT.to_le_bytes());
        h.update(OAM_VERSION.as_bytes());
        h.update([0]);
        h.update([Kind::Function.tag()]);
        h.update(crate::cjs::CJS_PARAMS.join(",").as_bytes());
        h.update([0]);
        h.update(b"x");
        assert_eq!(a.hash, hex32(&h.finalize().into()));
        // A different oam release keys the same source apart.
        let mut h = Sha256::new();
        h.update(v8::script_compiler::cached_data_version_tag().to_le_bytes());
        h.update(CODE_CACHE_FORMAT.to_le_bytes());
        h.update(b"0.0.0-other");
        h.update([0]);
        h.update([Kind::Function.tag()]);
        h.update(crate::cjs::CJS_PARAMS.join(",").as_bytes());
        h.update([0]);
        h.update(b"x");
        assert_ne!(a.hash, hex32(&h.finalize().into()));
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
        // Old and fresh seed-rejection tombstones.
        write_aged(
            &shard.join("old.seedrej"),
            BLOB_MAX_AGE + Duration::from_secs(1),
            now,
        );
        write_aged(&shard.join("new.seedrej"), Duration::from_secs(1), now);
        // An unrelated file is never touched.
        write_aged(&shard.join("notes.txt"), BLOB_MAX_AGE * 2, now);

        let report = sweep(&root, now);
        assert_eq!(
            report,
            SweepReport {
                tmp_files: 1,
                blobs: 1,
                seed_tombstones: 1
            }
        );
        assert!(!shard.join(".1.0.tmp").exists());
        assert!(shard.join(".2.0.tmp").exists());
        assert!(!shard.join("old.v8c").exists());
        assert!(shard.join("new.v8c").exists());
        assert!(!shard.join("old.seedrej").exists());
        assert!(shard.join("new.seedrej").exists());
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

    /// The stamp must ADVANCE when a sweep rewrites an existing empty one.
    /// Rust std emulates create+truncate on Windows as OPEN_ALWAYS plus a
    /// manual truncate, so rewriting a 0-byte file with 0 bytes never bumps
    /// NTFS's LastWriteTime: the stamp stayed at its first-ever write and
    /// every process that loaded a module re-walked the whole cache. The
    /// aged stamp here is 0 bytes, exactly what production leaves behind.
    #[test]
    fn sweep_advances_an_existing_empty_stamp() {
        let root = scratch("restamp");
        let stamp = root.join(SWEEP_STAMP);
        let now = SystemTime::now();
        std::fs::write(&stamp, b"").unwrap();
        let aged = now - SWEEP_INTERVAL * 3;
        std::fs::File::options()
            .write(true)
            .open(&stamp)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        assert!(sweep_due(&root, now), "an aged stamp is due");

        sweep(&root, now);
        let stamped = std::fs::metadata(&stamp).unwrap().modified().unwrap();
        assert!(
            stamped > aged + SWEEP_INTERVAL,
            "the sweep must move the stamp forward (still {stamped:?})"
        );
        assert!(
            !sweep_due(&root, now + Duration::from_secs(60)),
            "the next check within the interval must skip the walk"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Probe-first ordering: when the stamp cannot be written (a read-only
    /// root -- simulated portably by a DIRECTORY at the stamp path), the walk
    /// must not run at all. Deletions here WOULD succeed, so surviving files
    /// prove the skip, not permissions.
    #[test]
    fn sweep_skips_the_walk_when_the_stamp_cannot_be_written() {
        let root = scratch("nostamp");
        std::fs::create_dir_all(root.join(SWEEP_STAMP)).unwrap();
        let shard = root.join("ab");
        std::fs::create_dir_all(&shard).unwrap();
        let now = SystemTime::now();
        write_aged(&shard.join(".1.0.tmp"), TMP_MAX_AGE * 2, now);
        write_aged(
            &shard.join("old.v8c"),
            BLOB_MAX_AGE + Duration::from_secs(1),
            now,
        );
        assert_eq!(sweep(&root, now), SweepReport::default());
        assert!(
            shard.join(".1.0.tmp").exists() && shard.join("old.v8c").exists(),
            "an unwritable root must not be walked"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The compiled-binary trust order: a seeded key serves the SEED even
    /// when a disk entry coexists (the embedded blob is the trusted copy; a
    /// blob planted in the user-writable cache dir must not decide what a
    /// compiled binary runs), until a recorded rejection flips that key to
    /// the refreshed disk blob.
    #[test]
    fn load_prefers_seed_until_a_rejection_is_recorded() {
        let _rt = crate::JsRuntime::new();
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = format!("seedfirst-{}-{nanos}", std::process::id());
        let key = key_for(&source, Kind::Function, env_root_arc());
        seed(&source, Kind::Function, b"seed-bytes".to_vec());
        store(&key, b"disk-bytes");
        let got = load(&key).expect("seeded key loads");
        assert!(got.seeded, "the seed must win over a coexisting disk entry");
        assert_eq!(got.bytes, b"seed-bytes");

        record_seed_rejection(&key);
        assert!(
            tombstone_path(&key).is_file(),
            "a rejection leaves a tombstone"
        );
        let got = load(&key).expect("disk blob loads");
        assert!(
            !got.seeded,
            "after a recorded rejection the refreshed disk blob supersedes"
        );
        assert_eq!(got.bytes, b"disk-bytes");

        // The in-process half: the seed was forgotten, so with the disk
        // entries gone nothing serves the rejected bytes again.
        let _ = std::fs::remove_file(path_for_key(&key));
        let _ = std::fs::remove_file(tombstone_path(&key));
        assert!(load(&key).is_none());
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
