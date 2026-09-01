//! The oam type-check daemon (Gradle model): one resident process per
//! tsconfig project, serving `oam check` over loopback TCP.
//!
//! What it buys today (probed against tsgo 7.0.0-dev on a 300-file
//! project): repeat checks with an unchanged tree return the cached ODIF
//! instantly (the agent-loop case: re-check after a step that touched no
//! TS files); changed trees pay one warm incremental tsgo run (~300ms vs
//! ~500ms cold) with the build-info kept under oam's cache. tsgo's LSP
//! advertises workspaceDiagnostics:false and the @typescript/api ships at
//! TS 7.1 — when either matures, in-process incremental checking slots in
//! behind this same client protocol.
//!
//! Reliability contract: the daemon may NEVER make `oam check` worse than
//! one-shot — neither slower by more than a bounded margin nor wrong.
//! Every client-side failure (spawn, connect, version skew, protocol,
//! timeout) degrades silently to the one-shot path; `OAM_DEBUG=1` in
//! oam_cli prints the swallowed reason. The cache is keyed by a fingerprint
//! of EXACTLY the files tsgo's program reads (`--listFilesOnly`, refreshed
//! on every fill) plus a widened walk of the tsconfig dir (new files), the
//! tsconfig `extends`/`references` chain, package.json and the lockfiles —
//! so an edit to anything that can change the diagnostics misses the cache.
//!
//! Threads: the main thread only accepts, reads one request, answers
//! Ping/Shutdown in O(1), and hands EVERY Check to ONE check worker — the
//! worker probes the cache itself, so the accept thread never runs the
//! fingerprint walk (up to 50k stats on a large tree, which would starve a
//! Ping past the client's 2s budget and make it spawn a duplicate daemon).
//! A Ping is answered even while a minutes-long check runs, and a client
//! that queues behind a fill of the same tree gets that fill's result
//! instead of a second tsgo run. A watchdog thread implements the idle
//! shutdown by sending Shutdown to our own listener.
//!
//! Protocol: one JSONL request line in, one response line out, connection
//! per request. The token (in a user-only state file) authenticates that
//! the thing on the recorded port is OUR daemon — loopback port numbers
//! get reused by other software. Clients Ping (2s) before committing to
//! the long Check wait, so a stale port held by something that accepts and
//! never answers costs 2s, not the whole check budget.
//!
//! Sidecar files next to the state file: `<state>.error` (why the last
//! serve() failed during setup) and `<state>.spawn-failed` (timestamp of
//! the last spawn that never came up; clients skip spawning for 5 minutes
//! after it). `oam daemon status` reports both.

use crate::{Diagnostic, TsgoHandle, find_tsconfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SPAWN_WAIT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const PING_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// Server-side bound on reading one request line / writing one response.
const REQUEST_IO_TIMEOUT: Duration = Duration::from_secs(10);
/// After a spawn that never came up, don't try again for this long.
const SPAWN_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// A daemon Check is bounded by two tsgo runs (the file listing and the
/// check itself) plus slack; the client waits that long, never longer.
fn check_timeout() -> Duration {
    crate::tsgo_timeout() * 2 + Duration::from_secs(30)
}

/// 30 minutes, overridable (tests use a short value so any spawn/lifecycle
/// regression fails in seconds instead of wedging a suite for half an hour).
fn idle_shutdown() -> Duration {
    std::env::var("OAM_DAEMON_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(30 * 60))
}

#[derive(Serialize, Deserialize)]
struct StateFile {
    pid: u32,
    port: u16,
    token: String,
    version: String,
    tsconfig: String,
    /// The program the spawner's environment/project resolved to (unprobed
    /// spelling — see `crate::tsgo_lookup`); a client whose lookup differs
    /// retires this daemon.
    #[serde(default)]
    tsgo_lookup: String,
    /// The program that answered `--version`, and what it said.
    #[serde(default)]
    tsgo_path: String,
    #[serde(default)]
    tsgo_version: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum Request {
    Ping { token: String },
    Check { token: String },
    Shutdown { token: String },
}

#[derive(Default, Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Vec<Diagnostic>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checks_served: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_hits: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tsgo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tsgo_version: Option<String>,
}

impl Response {
    fn failure(message: impl Into<String>) -> Self {
        Response {
            ok: false,
            error: Some(message.into()),
            ..Default::default()
        }
    }

    fn checked(cached: bool, diagnostics: Vec<Diagnostic>) -> Self {
        Response {
            ok: true,
            cached: Some(cached),
            diagnostics: Some(diagnostics),
            ..Default::default()
        }
    }
}

/// The environment inputs [`resolve_cache_root`] decides from, read in one
/// place so the path logic itself stays pure and unit-testable.
pub(crate) struct CacheEnv {
    pub override_dir: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub temp_dir: PathBuf,
}

impl CacheEnv {
    fn from_process() -> Self {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        CacheEnv {
            override_dir: var("OAM_CACHE_DIR"),
            local_app_data: var("LOCALAPPDATA"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
            home: var("HOME"),
            temp_dir: std::env::temp_dir(),
        }
    }
}

/// Cache/state root: `OAM_CACHE_DIR` override (tests), else the platform
/// cache dir (`%LOCALAPPDATA%\oam`; `$XDG_CACHE_HOME/oam` or
/// `~/.cache/oam`), else `<temp>/oam` — NEVER the process cwd: with
/// LOCALAPPDATA/HOME unset (env -i, hermetic sandboxes, root units) the old
/// "." fallback wrote build-info and daemon state into the user's project.
///
/// Mirrors `oam_engine::code_cache::cache_root` (and `crash.rs`), kept as
/// separate copies so the v8-bound engine never depends on the tsgo crate;
/// change both together.
pub(crate) fn resolve_cache_root(env: &CacheEnv) -> PathBuf {
    if let Some(dir) = &env.override_dir {
        return dir.clone();
    }
    let platform = if cfg!(windows) {
        env.local_app_data.as_ref().map(|d| d.join("oam"))
    } else {
        env.xdg_cache_home
            .as_ref()
            .map(|d| d.join("oam"))
            .or_else(|| env.home.as_ref().map(|h| h.join(".cache").join("oam")))
    };
    platform.unwrap_or_else(|| env.temp_dir.join("oam"))
}

pub(crate) fn cache_root() -> PathBuf {
    resolve_cache_root(&CacheEnv::from_process())
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Stable per-project key from the canonicalized tsconfig path: the
/// PARENT dir is canonicalized (symlinks, Windows case) and the file name
/// re-joined, so two spellings of one project share a daemon and a
/// build-info file, while two projects that share a symlinked tsconfig do
/// NOT collapse into one daemon with the wrong root. Falls back to the
/// absolute path when the dir does not exist.
pub(crate) fn project_key(tsconfig: &Path) -> String {
    let absolute = std::path::absolute(tsconfig).unwrap_or_else(|_| tsconfig.to_path_buf());
    let canonical = match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|dir| dir.join(name))
            .unwrap_or(absolute),
        _ => absolute,
    };
    format!("{:016x}", fnv1a64(canonical.to_string_lossy().as_bytes()))
}

fn state_path(tsconfig: &Path) -> PathBuf {
    cache_root()
        .join("ts-daemons")
        .join(format!("{}.json", project_key(tsconfig)))
}

/// `<state file>.<suffix>`.
fn sidecar(state_file: &Path, suffix: &str) -> PathBuf {
    let mut name = state_file.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn mark_spawn_failed(state_file: &Path) {
    if let Some(dir) = state_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        sidecar(state_file, "spawn-failed"),
        now_millis().to_string(),
    );
}

/// How long ago the last spawn failure was recorded, if one is.
fn spawn_failure_age(state_file: &Path) -> Option<Duration> {
    let raw = std::fs::read_to_string(sidecar(state_file, "spawn-failed")).ok()?;
    let stamp: u128 = raw.trim().parse().ok()?;
    Some(Duration::from_millis(
        u64::try_from(now_millis().saturating_sub(stamp)).unwrap_or(u64::MAX),
    ))
}

fn last_error(state_file: &Path) -> Option<String> {
    std::fs::read_to_string(sidecar(state_file, "error"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Good-enough loopback auth: the secret lives in a user-only file anyway,
/// so the token's job is daemon identity (port reuse), not cryptography.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut token = String::with_capacity(64);
    for _ in 0..4 {
        let hash = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        token.push_str(&format!("{hash:016x}"));
    }
    token
}

// ------------------------------------------------------------ fingerprint --

/// Extensions tsc can load (allowJs/checkJs, resolveJsonModule; `.d.ts`
/// and `.d.mts` fall under ts/mts).
fn tsc_loadable(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e,
                "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "json"
            )
        })
}

const LOCKFILES: [&str; 5] = [
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "node_modules/.package-lock.json",
];

/// A file's identity for the fingerprint; `None` = missing.
type Stamp = Option<(u128, u64)>;

fn stamp(meta: std::io::Result<std::fs::Metadata>) -> Stamp {
    let meta = meta.ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime, meta.len()))
}

/// Whole-project content fingerprint, FNV-mixed over (path, mtime, size)
/// of: every file in `listed` (tsgo's own program file list — covers
/// allowJs/checkJs, .json modules, @types, tsgo's lib .d.ts, and
/// extends/include targets outside the tsconfig dir; a listed file that
/// has since vanished hashes as "missing"), plus a walk of the tsconfig
/// dir over every tsc-loadable extension (catches NEW files the last
/// listing predates; skips node_modules/.git/.oam anywhere and `target`
/// only directly under the root), plus the parent DIRECTORIES of listed
/// files outside the root (a dir's mtime changes on create/delete on
/// NTFS/ext4/APFS, so a file ADDED under an out-of-root include/references
/// dir — which no walk covers — still misses the cache), plus the tsconfig
/// `extends`/`references` chain, plus package.json and the lockfiles on
/// every ancestor (dependency upgrades). None = too big to fingerprint
/// (then we simply never serve from cache — correctness first).
///
/// Staleness envelope (probed): a same-size rewrite within the mtime
/// granularity window would produce the same fingerprint. Tier-1
/// filesystems are ns-class (NTFS 100ns — probed unreproducible with
/// back-to-back writes; ext4/APFS ns), so this only matters on FAT/exFAT
/// (2s) or some network mounts. Revisit with content hashing if a real
/// report ever lands.
///
/// Concurrent-spawn note (probed): racing `oam check` calls can spawn
/// sibling daemons; the state file is last-writer-wins and orphans
/// self-reap after one idle period having served at most their own
/// spawner correctly. Benign by design — no wrong results possible, and
/// an orphan's exit only removes the state file it still owns.
fn fingerprint(root: &Path, tsconfig: &Path, listed: &[PathBuf]) -> Option<u64> {
    const CAP: usize = 50_000;
    let mut entries: BTreeMap<PathBuf, Stamp> = BTreeMap::new();

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir).ok()?;
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                let skip = matches!(name.as_ref(), "node_modules" | ".git" | ".oam")
                    || (name == "target" && dir == root);
                if !skip {
                    stack.push(path);
                }
                continue;
            }
            if name != "tsconfig.json" && !tsc_loadable(&name) {
                continue;
            }
            entries.insert(path, stamp(entry.metadata()));
            if entries.len() > CAP {
                return None;
            }
        }
    }

    for path in listed {
        entries.insert(path.clone(), stamp(std::fs::metadata(path)));
        // A listed file OUTSIDE the walked root (out-of-root include /
        // references / extends targets, @types and lib dirs): stamp its
        // parent DIRECTORY too — the dir mtime changes when a sibling is
        // created or deleted, so `../shared/new.ts` invalidates the cache
        // even though no listing names it yet and no walk reaches it.
        if !path.starts_with(root)
            && let Some(parent) = path.parent()
        {
            entries
                .entry(parent.to_path_buf())
                .or_insert_with(|| stamp(std::fs::metadata(parent)));
        }
    }
    for path in tsconfig_chain(tsconfig) {
        entries.insert(path.clone(), stamp(std::fs::metadata(&path)));
    }
    for ancestor in root.ancestors() {
        for name in LOCKFILES {
            let path = ancestor.join(name);
            if let Ok(meta) = std::fs::metadata(&path) {
                entries.insert(path, stamp(Ok(meta)));
            }
        }
    }
    if entries.len() > CAP {
        return None;
    }

    let mut hash: u64 = 0xcbf29ce484222325;
    for (path, stamp) in entries {
        let (mtime, len) = stamp.unwrap_or((u128::MAX, u64::MAX));
        hash ^= fnv1a64(path.to_string_lossy().as_bytes()) ^ (mtime as u64) ^ len.rotate_left(32);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(hash)
}

/// `tsconfig` plus every config it `extends` (string or array, relative or
/// bare-package) and every project it `references`, transitively, cycle-
/// guarded. Best-effort: an unparseable config contributes itself only.
fn tsconfig_chain(tsconfig: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut queue = vec![tsconfig.to_path_buf()];
    while let Some(config) = queue.pop() {
        if out.contains(&config) || out.len() >= 64 {
            continue;
        }
        out.push(config.clone());
        let Some(dir) = config.parent() else { continue };
        let Ok(raw) = std::fs::read_to_string(&config) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(&raw)) else {
            continue;
        };
        let extends: Vec<&str> = match json.get("extends") {
            Some(serde_json::Value::String(s)) => vec![s.as_str()],
            Some(serde_json::Value::Array(items)) => {
                items.iter().filter_map(|v| v.as_str()).collect()
            }
            _ => Vec::new(),
        };
        for spec in extends {
            if let Some(path) = resolve_extends(dir, spec) {
                queue.push(path);
            }
        }
        if let Some(serde_json::Value::Array(refs)) = json.get("references") {
            for reference in refs {
                if let Some(path) = reference.get("path").and_then(|v| v.as_str()) {
                    let path = crate::normalize_path(&dir.join(path));
                    queue.push(if path.is_dir() {
                        path.join("tsconfig.json")
                    } else {
                        path
                    });
                }
            }
        }
    }
    out
}

/// TypeScript's `extends` resolution, minus package.json `tsconfig` fields:
/// `./x` / `../x` / absolute -> that file (`.json` appended if needed, a dir
/// means its tsconfig.json); anything else -> `node_modules/<spec>` walking
/// up.
fn resolve_extends(dir: &Path, spec: &str) -> Option<PathBuf> {
    fn existing(base: PathBuf) -> Option<PathBuf> {
        // Lexically folded (`pkg/../base.json` -> `base.json`) so one file
        // reached through two configs dedupes to one fingerprint entry.
        let base = crate::normalize_path(&base);
        if base.is_file() {
            return Some(base);
        }
        if base.is_dir() {
            let nested = base.join("tsconfig.json");
            return nested.is_file().then_some(nested);
        }
        let mut with_json = base.into_os_string();
        with_json.push(".json");
        let with_json = PathBuf::from(with_json);
        with_json.is_file().then_some(with_json)
    }
    let relative = spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with('/')
        || Path::new(spec).is_absolute();
    if relative {
        return existing(dir.join(spec));
    }
    let mut probe = dir.to_path_buf();
    loop {
        if let Some(found) = existing(probe.join("node_modules").join(spec)) {
            return Some(found);
        }
        if !probe.pop() {
            return None;
        }
    }
}

/// tsconfig.json is JSONC: drop a BOM, `//` and `/* */` comments, and
/// trailing commas before serde sees it. Twin of the loader's
/// `oam_loader::tsconfig::strip_jsonc`, duplicated for the same
/// no-dependency reason as `find_tsconfig`.
fn strip_jsonc(input: &str) -> String {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                out.push(c);
                i += 1;
                while i < chars.len() {
                    let d = chars[i];
                    out.push(d);
                    i += 1;
                    if d == '\\' && i < chars.len() {
                        out.push(chars[i]);
                        i += 1;
                    } else if d == '"' {
                        break;
                    }
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------- client --

#[derive(Debug, Serialize)]
pub struct DaemonStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks_served: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hits: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsgo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsgo_version: Option<String>,
    /// Why the last daemon start failed (from `<state>.error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Milliseconds since the last spawn that never came up (from
    /// `<state>.spawn-failed`); spawns are skipped for 5 minutes after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_failed_ms_ago: Option<u64>,
}

fn read_state(tsconfig: &Path) -> Option<StateFile> {
    let raw = std::fs::read_to_string(state_path(tsconfig)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// One request/response round trip to `port`. `read_timeout` is the whole
/// budget for the answer; the connect is always 500ms.
fn send(port: u16, request: &Request, read_timeout: Duration) -> Option<Response> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(read_timeout)).ok()?;
    stream.set_write_timeout(Some(REQUEST_IO_TIMEOUT)).ok()?;
    // One line, one write: the payload and its newline in a single buffer
    // (a `writeln!` on a raw stream issues them as two syscalls).
    let mut line = serde_json::to_string(request).ok()?;
    line.push('\n');
    (&stream).write_all(line.as_bytes()).ok()?;
    let mut response = String::new();
    BufReader::new(&stream).read_line(&mut response).ok()?;
    serde_json::from_str(&response).ok()
}

fn request(state: &StateFile, request: &Request, read_timeout: Duration) -> Option<Response> {
    send(state.port, request, read_timeout)
}

/// Check `target`'s project through the daemon, spawning it if needed.
/// Err = use the one-shot path instead (NEVER a user-facing failure); the
/// string is the reason, for `OAM_DEBUG`.
pub fn check_via_daemon(target: &Path) -> Result<Vec<Diagnostic>, String> {
    let target = std::path::absolute(target).map_err(|e| e.to_string())?;
    let tsconfig = find_tsconfig(&target).ok_or("no tsconfig: single-file checks stay one-shot")?;

    if let Some(result) = try_existing(&tsconfig) {
        return result;
    }
    let state_file = state_path(&tsconfig);
    if let Some(age) = spawn_failure_age(&state_file) {
        if age < SPAWN_BACKOFF {
            return Err(format!(
                "daemon spawn failed {}s ago; not retrying for {}s (see `oam daemon status`)",
                age.as_secs(),
                SPAWN_BACKOFF.as_secs()
            ));
        }
        let _ = std::fs::remove_file(sidecar(&state_file, "spawn-failed"));
    }
    let mut child = spawn_daemon(&tsconfig)?;
    let deadline = Instant::now() + SPAWN_WAIT;
    while Instant::now() < deadline {
        // A daemon that dies during setup is reported at once (with the
        // reason it wrote), not after the full wait.
        if let Ok(Some(status)) = child.try_wait() {
            mark_spawn_failed(&state_file);
            let reason = last_error(&state_file)
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            return Err(format!("daemon exited during startup ({status}){reason}"));
        }
        if let Some(result) = try_existing(&tsconfig) {
            return result;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    mark_spawn_failed(&state_file);
    Err(format!(
        "daemon did not come up within {}s",
        SPAWN_WAIT.as_secs()
    ))
}

/// None = no usable daemon (caller spawns). Some(Err) = the daemon answered
/// but could not check (caller falls back without spawning).
fn try_existing(tsconfig: &Path) -> Option<Result<Vec<Diagnostic>, String>> {
    let state = read_state(tsconfig)?;
    let root = tsconfig.parent().unwrap_or(tsconfig);
    if state.version != env!("CARGO_PKG_VERSION") || state.tsgo_lookup != crate::tsgo_lookup(root) {
        // Version skew (oam upgraded, or the tsgo this project resolves to
        // changed): retire the old daemon, caller respawns.
        retire(&state, tsconfig);
        return None;
    }
    // Liveness first, on a short leash: a crashed daemon whose port is now
    // held by something that accepts and never answers must cost 2s, not
    // the whole check budget. A daemon mid-check still answers (the check
    // runs on its worker thread).
    let alive = request(
        &state,
        &Request::Ping {
            token: state.token.clone(),
        },
        PING_TIMEOUT,
    )
    .is_some_and(|r| r.ok);
    if !alive {
        return None;
    }
    let response = request(
        &state,
        &Request::Check {
            token: state.token.clone(),
        },
        check_timeout(),
    )?;
    if response.ok {
        Some(Ok(response.diagnostics.unwrap_or_default()))
    } else {
        Some(Err(response
            .error
            .unwrap_or_else(|| "daemon returned ok:false".into())))
    }
}

fn retire(state: &StateFile, tsconfig: &Path) {
    let _ = request(
        state,
        &Request::Shutdown {
            token: state.token.clone(),
        },
        SHUTDOWN_TIMEOUT,
    );
    let _ = std::fs::remove_file(state_path(tsconfig));
}

pub fn status(target: &Path) -> DaemonStatus {
    let mut status = DaemonStatus {
        running: false,
        pid: None,
        port: None,
        version: None,
        checks_served: None,
        cache_hits: None,
        tsgo: None,
        tsgo_version: None,
        last_error: None,
        spawn_failed_ms_ago: None,
    };
    let Ok(target) = std::path::absolute(target) else {
        return status;
    };
    let Some(tsconfig) = find_tsconfig(&target) else {
        return status;
    };
    let state_file = state_path(&tsconfig);
    status.last_error = last_error(&state_file);
    status.spawn_failed_ms_ago = spawn_failure_age(&state_file)
        .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
    let Some(state) = read_state(&tsconfig) else {
        return status;
    };
    if let Some(response) = request(
        &state,
        &Request::Ping {
            token: state.token.clone(),
        },
        PING_TIMEOUT,
    )
    .filter(|r| r.ok)
    {
        status.running = true;
        status.pid = Some(state.pid);
        status.port = Some(state.port);
        status.version = response.version;
        status.checks_served = response.checks_served;
        status.cache_hits = response.cache_hits;
        status.tsgo = response.tsgo;
        status.tsgo_version = response.tsgo_version;
    }
    status
}

/// Stop the project's daemon if one is running. Returns whether one was.
/// A daemon that does not answer Shutdown within 2s is killed by its
/// recorded pid (after checking that pid still names an oam process, so a
/// recycled pid is never shot); the state file goes either way.
pub fn stop(target: &Path) -> bool {
    let Ok(target) = std::path::absolute(target) else {
        return false;
    };
    let Some(tsconfig) = find_tsconfig(&target) else {
        return false;
    };
    let Some(state) = read_state(&tsconfig) else {
        return false;
    };
    let stopped = request(
        &state,
        &Request::Shutdown {
            token: state.token.clone(),
        },
        SHUTDOWN_TIMEOUT,
    )
    .is_some_and(|r| r.ok);
    let killed = !stopped && kill_daemon(state.pid);
    let _ = std::fs::remove_file(state_path(&tsconfig));
    stopped || killed
}

/// Kill a wedged daemon by pid with OS tools (no libc/oam_core dependency
/// here). Refuses when the pid no longer names an oam process.
fn kill_daemon(pid: u32) -> bool {
    use std::process::{Command, Stdio};
    let pid_str = pid.to_string();
    let quiet = |command: &mut Command| {
        command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped());
    };
    let image = if cfg!(windows) {
        let mut command = Command::new("tasklist");
        command.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        quiet(&mut command);
        command.output()
    } else {
        let mut command = Command::new("ps");
        command.args(["-p", &pid_str, "-o", "comm="]);
        quiet(&mut command);
        command.output()
    };
    let is_oam = image.is_ok_and(|out| {
        String::from_utf8_lossy(&out.stdout)
            .to_ascii_lowercase()
            .contains("oam")
    });
    if !is_oam {
        return false;
    }
    let status = if cfg!(windows) {
        let mut command = Command::new("taskkill");
        command.args(["/T", "/F", "/PID", &pid_str]);
        quiet(&mut command);
        command.status()
    } else {
        let mut command = Command::new("kill");
        command.args(["-KILL", &pid_str]);
        quiet(&mut command);
        command.status()
    };
    status.is_ok_and(|s| s.success())
}

/// Windows: a spawned child inherits EVERY inheritable handle in our table,
/// not just its configured stdio. When `oam check` itself runs with piped
/// stdio (cargo test, CI, agents), those pipe write-handles leak into the
/// detached daemon, and the parent's `.output()` blocks until the DAEMON
/// exits — observed as a 30-minute test hang. Clearing the inherit flag on
/// our own std handles before the spawn closes the leak; our own IO is
/// unaffected (the flag only governs inheritance).
///
/// IMPORTANT: every INTERMEDIATE spawner needs this too — `oam mcp` piping
/// `oam run` leaked ITS std handles (the agent's pipes) into the run child
/// as non-stdio extras, which the daemon then inherited transitively. Any
/// oam code spawning children with piped/null stdio should call this first
/// (and must NOT rely on Stdio::inherit afterwards — inherit requires the
/// handle to stay inheritable). The durable fix is an explicit
/// PROC_THREAD_ATTRIBUTE_HANDLE_LIST; tracked for the perf/correctness pass.
#[cfg(windows)]
pub fn unshare_std_handles() {
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut core::ffi::c_void;
        fn SetHandleInformation(handle: *mut core::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for std_id in [-10i32, -11, -12] {
        // SAFETY: GetStdHandle/SetHandleInformation on our own process's
        // std handles; null/INVALID_HANDLE_VALUE are guarded, and a failed
        // SetHandleInformation is harmless (worst case: the old behavior).
        unsafe {
            let handle = GetStdHandle(std_id as u32);
            if !handle.is_null() && handle as isize != -1 {
                let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Non-Windows: handle inheritance is governed by CLOEXEC, which Rust std
/// sets on everything it creates; nothing to do.
#[cfg(not(windows))]
pub fn unshare_std_handles() {}

/// Spawn the daemon process. The returned `Child` is only ever `try_wait`ed
/// (an early exit means setup failed); dropping it does not kill it.
fn spawn_daemon(tsconfig: &Path) -> Result<std::process::Child, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("__oamd-ts")
        .arg(tsconfig)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Test/ops overrides (OAM_CACHE_DIR, OAM_DAEMON_IDLE_MS, OAM_TSGO,
    // OAM_TSGO_TIMEOUT_MS) reach the daemon by plain inheritance: a
    // std::process::Command passes the whole parent environment through
    // unless env_clear is called, which nothing here does.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        unshare_std_handles();
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    command.spawn().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- server --

/// State shared by the accept loop, the check worker and the watchdog.
struct Shared {
    token: String,
    tsconfig: PathBuf,
    root: PathBuf,
    tsgo_path: String,
    tsgo_version: String,
    /// The in-flight tsgo of the worker; Shutdown cancels it.
    handle: TsgoHandle,
    cache: Mutex<Cache>,
    checks_served: AtomicU64,
    cache_hits: AtomicU64,
    /// A check is running: the watchdog must not idle-reap under it.
    busy: AtomicBool,
    last_activity: Mutex<Instant>,
}

#[derive(Default)]
struct Cache {
    entry: Option<(u64, Vec<Diagnostic>)>,
    /// tsgo's program file list as of the last fill.
    listed: Vec<PathBuf>,
}

impl Shared {
    fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .elapsed()
    }

    /// The cached diagnostics, if the tree still matches the last fill.
    fn cached_if_fresh(&self) -> Option<Vec<Diagnostic>> {
        let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        let (fp, diagnostics) = cache.entry.as_ref()?;
        (fingerprint(&self.root, &self.tsconfig, &cache.listed) == Some(*fp))
            .then(|| diagnostics.clone())
    }

    /// Cache miss: refresh tsgo's file list, fingerprint BEFORE the check
    /// (an edit during the run must miss next time), run it, store.
    fn fill(&self) -> Result<Vec<Diagnostic>, Diagnostic> {
        let listed = crate::list_files(&self.tsconfig, &self.handle).ok();
        let fp = listed
            .as_deref()
            .and_then(|listed| fingerprint(&self.root, &self.tsconfig, listed));
        let diagnostics = crate::check_cancellable(&self.tsconfig, &self.handle)?;
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        match (fp, listed) {
            (Some(fp), Some(listed)) => {
                cache.entry = Some((fp, diagnostics.clone()));
                cache.listed = listed;
            }
            _ => cache.entry = None,
        }
        Ok(diagnostics)
    }
}

/// Daemon main loop. Runs until shutdown request or idle timeout; the state
/// file is written only once the listener is live (clients poll for it).
/// Any setup failure is also recorded in `<state>.error` for
/// `oam daemon status`.
pub fn serve(tsconfig: &Path) -> std::io::Result<()> {
    let tsconfig = std::path::absolute(tsconfig)?;
    let state_file = state_path(&tsconfig);
    std::fs::create_dir_all(state_file.parent().expect("state dir"))?;
    match serve_inner(&tsconfig, &state_file) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::write(sidecar(&state_file, "error"), e.to_string());
            Err(e)
        }
    }
}

fn serve_inner(tsconfig: &Path, state_file: &Path) -> std::io::Result<()> {
    let root = tsconfig
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // Prove the tsgo this project resolves to before advertising ourselves:
    // a daemon with no usable compiler must fail setup (recorded), not
    // serve OAM-TS0002 for its whole idle life.
    let handle = TsgoHandle::new();
    let (tsgo_path, tsgo_version) =
        crate::resolve_tsgo(&root, &handle).map_err(|d| std::io::Error::other(d.message))?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let token = random_token();

    let state = StateFile {
        pid: std::process::id(),
        port,
        token: token.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        tsconfig: tsconfig.to_string_lossy().into_owned(),
        tsgo_lookup: crate::tsgo_lookup(&root),
        tsgo_path: tsgo_path.to_string_lossy().into_owned(),
        tsgo_version: tsgo_version.clone(),
    };
    std::fs::write(state_file, serde_json::to_string(&state)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(state_file, std::fs::Permissions::from_mode(0o600));
    }
    // We are up: whatever the previous attempt recorded is history.
    let _ = std::fs::remove_file(sidecar(state_file, "error"));
    let _ = std::fs::remove_file(sidecar(state_file, "spawn-failed"));

    let shared = Arc::new(Shared {
        token,
        tsconfig: tsconfig.to_path_buf(),
        root,
        tsgo_path: state.tsgo_path.clone(),
        tsgo_version,
        handle,
        cache: Mutex::new(Cache::default()),
        checks_served: AtomicU64::new(0),
        cache_hits: AtomicU64::new(0),
        busy: AtomicBool::new(false),
        last_activity: Mutex::new(Instant::now()),
    });
    let (tx, rx) = std::sync::mpsc::channel::<TcpStream>();
    {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || check_worker(rx, shared));
    }
    {
        let shared = Arc::clone(&shared);
        let state_file = state_file.to_path_buf();
        std::thread::spawn(move || idle_watchdog(port, shared, state_file));
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            // Transient accept failure (EMFILE and friends): don't spin.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        shared.touch();
        if !handle_connection(stream, &shared, &tx) {
            // Shutdown: take the in-flight tsgo (if any) down with us, and
            // remove the state file only if it is still OURS — a sibling
            // spawned by a racing client may have overwritten it.
            shared.handle.cancel();
            remove_state_if_owned(state_file);
            return Ok(());
        }
    }
    Ok(())
}

/// Delete the state file only when it still records our pid.
fn remove_state_if_owned(state_file: &Path) {
    let owned = std::fs::read_to_string(state_file)
        .ok()
        .and_then(|raw| serde_json::from_str::<StateFile>(&raw).ok())
        .is_some_and(|state| state.pid == std::process::id());
    if owned {
        let _ = std::fs::remove_file(state_file);
    }
}

/// Idle shutdown: once nothing has arrived for `OAM_DAEMON_IDLE_MS` and no
/// check is running, ask our own accept loop to exit (it owns the state
/// file); if that somehow fails, exit directly.
fn idle_watchdog(port: u16, shared: Arc<Shared>, state_file: PathBuf) {
    let idle = idle_shutdown();
    let tick = idle
        .min(Duration::from_secs(1))
        .max(Duration::from_millis(10));
    loop {
        std::thread::sleep(tick);
        if shared.busy.load(Ordering::SeqCst) || shared.idle_for() <= idle {
            continue;
        }
        let answered = send(
            port,
            &Request::Shutdown {
                token: shared.token.clone(),
            },
            SHUTDOWN_TIMEOUT,
        )
        .is_some_and(|r| r.ok);
        if !answered {
            shared.handle.cancel();
            remove_state_if_owned(&state_file);
            std::process::exit(0);
        }
        return;
    }
}

fn respond(stream: &TcpStream, response: &Response) {
    if let Ok(mut line) = serde_json::to_string(response) {
        line.push('\n');
        let _ = (&*stream).write_all(line.as_bytes());
    }
}

/// Every Check, one at a time, off the accept thread — the cache probe runs
/// here too (a fingerprint walk on the accept thread would starve Ping). A
/// client that was queued behind a fill of the same tree is served from
/// that fill.
fn check_worker(rx: Receiver<TcpStream>, shared: Arc<Shared>) {
    for stream in rx {
        shared.busy.store(true, Ordering::SeqCst);
        if let Some(diagnostics) = shared.cached_if_fresh() {
            shared.cache_hits.fetch_add(1, Ordering::Relaxed);
            respond(&stream, &Response::checked(true, diagnostics));
        } else {
            match shared.fill() {
                Ok(diagnostics) => respond(&stream, &Response::checked(false, diagnostics)),
                Err(failure) => respond(&stream, &Response::failure(failure.to_jsonl())),
            }
        }
        shared.busy.store(false, Ordering::SeqCst);
        shared.touch();
    }
}

/// Returns false when the daemon should exit (shutdown request).
fn handle_connection(stream: TcpStream, shared: &Shared, worker: &Sender<TcpStream>) -> bool {
    let _ = stream.set_read_timeout(Some(REQUEST_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_IO_TIMEOUT));
    let _ = stream.set_nodelay(true);
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return true;
    }

    let Ok(request) = serde_json::from_str::<Request>(line.trim()) else {
        respond(&stream, &Response::failure("malformed request"));
        return true;
    };
    let presented = match &request {
        Request::Ping { token } | Request::Check { token } | Request::Shutdown { token } => token,
    };
    if *presented != shared.token {
        respond(&stream, &Response::failure("bad token"));
        return true;
    }

    match request {
        Request::Ping { .. } => {
            respond(
                &stream,
                &Response {
                    ok: true,
                    version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    checks_served: Some(shared.checks_served.load(Ordering::Relaxed)),
                    cache_hits: Some(shared.cache_hits.load(Ordering::Relaxed)),
                    tsgo: Some(shared.tsgo_path.clone()),
                    tsgo_version: Some(shared.tsgo_version.clone()),
                    ..Default::default()
                },
            );
            true
        }
        Request::Shutdown { .. } => {
            respond(
                &stream,
                &Response {
                    ok: true,
                    ..Default::default()
                },
            );
            false
        }
        Request::Check { .. } => {
            shared.checks_served.fetch_add(1, Ordering::Relaxed);
            // Always queued, never probed here: cached_if_fresh runs the
            // full fingerprint walk, and on a large tree that would hold
            // the accept thread (and the cache mutex) past the next Ping's
            // 2s budget — the client would conclude the daemon is dead and
            // spawn a duplicate. The worker re-probes the cache first, so a
            // hit still skips tsgo; accept-thread latency stays O(1).
            if let Err(std::sync::mpsc::SendError(stream)) = worker.send(stream) {
                respond(&stream, &Response::failure("daemon check worker is gone"));
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("oam-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b"oam"), fnv1a64(b"oam"));
        assert_ne!(fnv1a64(b"oam"), fnv1a64(b"oan"));
    }

    #[test]
    fn fingerprint_changes_on_edit() {
        let dir = scratch("fp");
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, "{}").unwrap();
        std::fs::write(dir.join("a.ts"), "export const a = 1;").unwrap();
        let before = fingerprint(&dir, &tsconfig, &[]).expect("fingerprints");
        // Size change guarantees a new fingerprint even on coarse mtimes.
        std::fs::write(dir.join("a.ts"), "export const a = 12;").unwrap();
        let after = fingerprint(&dir, &tsconfig, &[]).expect("fingerprints");
        assert_ne!(before, after);
        // Irrelevant files don't participate.
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        assert_eq!(after, fingerprint(&dir, &tsconfig, &[]).unwrap());
    }

    #[test]
    fn fingerprint_covers_checkjs_json_nested_target_and_new_files() {
        // The blind spots that served stale clean results: .js under
        // checkJs, .json modules, anything under a nested `target` dir,
        // and a file added after the last listing.
        let dir = scratch("fp-wide");
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, "{}").unwrap();
        std::fs::create_dir_all(dir.join("src").join("target")).unwrap();
        std::fs::write(dir.join("src").join("j.js"), "x").unwrap();
        std::fs::write(dir.join("src").join("d.json"), "{}").unwrap();
        std::fs::write(dir.join("src").join("target").join("t.ts"), "x").unwrap();
        let base = fingerprint(&dir, &tsconfig, &[]).unwrap();

        std::fs::write(dir.join("src").join("j.js"), "xyz").unwrap();
        let js = fingerprint(&dir, &tsconfig, &[]).unwrap();
        assert_ne!(base, js, "checkJs .js edit must invalidate");

        std::fs::write(dir.join("src").join("d.json"), "{\"a\":1}").unwrap();
        let json = fingerprint(&dir, &tsconfig, &[]).unwrap();
        assert_ne!(js, json, ".json module edit must invalidate");

        std::fs::write(dir.join("src").join("target").join("t.ts"), "xyz").unwrap();
        let nested = fingerprint(&dir, &tsconfig, &[]).unwrap();
        assert_ne!(json, nested, "src/target is NOT a build dir");

        std::fs::write(dir.join("src").join("new.ts"), "x").unwrap();
        let added = fingerprint(&dir, &tsconfig, &[]).unwrap();
        assert_ne!(nested, added, "a new file must invalidate");

        // The root-level `target` (a Rust build dir) stays skipped.
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target").join("build.ts"), "x").unwrap();
        assert_eq!(added, fingerprint(&dir, &tsconfig, &[]).unwrap());
    }

    #[test]
    fn fingerprint_tracks_listed_files_outside_the_root_and_their_removal() {
        let dir = scratch("fp-listed");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let tsconfig = root.join("tsconfig.json");
        std::fs::write(&tsconfig, "{}").unwrap();
        let outside = dir.join("types.d.ts");
        std::fs::write(&outside, "declare const x: number;").unwrap();
        let listed = vec![outside.clone()];
        let before = fingerprint(&root, &tsconfig, &listed).unwrap();
        // Different length on purpose: a same-size rewrite inside one mtime
        // clock tick is the documented staleness envelope (see fingerprint's
        // doc), and back-to-back test writes land in the same tick.
        std::fs::write(&outside, "declare const x: string | number;").unwrap();
        let edited = fingerprint(&root, &tsconfig, &listed).unwrap();
        assert_ne!(before, edited, "an @types/extends target outside the root");
        std::fs::remove_file(&outside).unwrap();
        let removed = fingerprint(&root, &tsconfig, &listed).unwrap();
        assert_ne!(edited, removed, "a vanished program file");
    }

    #[test]
    fn fingerprint_catches_files_added_under_out_of_root_dirs() {
        // Regression: tsconfig `"include": ["src", "../shared"]`, then
        // create ../shared/new.ts — the walk never leaves the root and no
        // listing names the new file, so only the parent-dir stamp of the
        // listed ../shared files can invalidate the cached result.
        let dir = scratch("fp-outside-add");
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let tsconfig = root.join("tsconfig.json");
        std::fs::write(&tsconfig, "{}").unwrap();
        let shared = dir.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let existing = shared.join("a.ts");
        std::fs::write(&existing, "export const a: number = 1;").unwrap();
        let listed = vec![existing.clone()];
        let before = fingerprint(&root, &tsconfig, &listed).unwrap();

        std::fs::write(shared.join("new.ts"), "export const b: string = 1;").unwrap();
        let added = fingerprint(&root, &tsconfig, &listed).unwrap();
        assert_ne!(before, added, "../shared/new.ts must invalidate");

        std::fs::remove_file(shared.join("new.ts")).unwrap();
        let removed = fingerprint(&root, &tsconfig, &listed).unwrap();
        assert_ne!(added, removed, "deleting it must invalidate again");
    }

    #[test]
    fn fingerprint_tracks_extends_targets_and_lockfiles() {
        let dir = scratch("fp-chain");
        let root = dir.join("pkg");
        std::fs::create_dir_all(&root).unwrap();
        let tsconfig = root.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            "{\n  // comment\n  \"extends\": \"../tsconfig.base\",\n  \"compilerOptions\": { \"strict\": true, },\n}",
        )
        .unwrap();
        let base_cfg = dir.join("tsconfig.base.json");
        std::fs::write(&base_cfg, "{\"compilerOptions\":{\"strict\":false}}").unwrap();
        let chain = tsconfig_chain(&tsconfig);
        assert!(chain.contains(&base_cfg), "chain: {chain:?}");

        let before = fingerprint(&root, &tsconfig, &[]).unwrap();
        std::fs::write(&base_cfg, "{\"compilerOptions\":{\"strict\":true}}").unwrap();
        let after = fingerprint(&root, &tsconfig, &[]).unwrap();
        assert_ne!(before, after, "an extends target above the root");

        // A lockfile above the root (monorepo layout) is watched too.
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: 9").unwrap();
        let locked = fingerprint(&root, &tsconfig, &[]).unwrap();
        assert_ne!(after, locked);
    }

    #[test]
    fn strip_jsonc_handles_comments_trailing_commas_and_strings() {
        let raw = "\u{feff}{ // c\n \"a\": \"x // not a comment\", /* b */ \"b\": [1, 2,], }";
        let parsed: serde_json::Value = serde_json::from_str(&strip_jsonc(raw)).unwrap();
        assert_eq!(parsed["a"], "x // not a comment");
        assert_eq!(parsed["b"], serde_json::json!([1, 2]));
    }

    #[test]
    fn project_keys_are_path_stable() {
        let a = project_key(Path::new("/p/tsconfig.json"));
        assert_eq!(a, project_key(Path::new("/p/tsconfig.json")));
        assert_ne!(a, project_key(Path::new("/q/tsconfig.json")));
    }

    #[test]
    fn project_key_collapses_spellings_of_one_existing_dir() {
        let dir = scratch("Key-Case");
        let tsconfig = dir.join("tsconfig.json");
        std::fs::write(&tsconfig, "{}").unwrap();
        let canonical = project_key(&tsconfig);
        #[cfg(windows)]
        {
            // NTFS is case-insensitive: the same dir spelled differently.
            let upper =
                PathBuf::from(dir.to_string_lossy().to_ascii_uppercase()).join("tsconfig.json");
            let lower =
                PathBuf::from(dir.to_string_lossy().to_ascii_lowercase()).join("tsconfig.json");
            assert_eq!(canonical, project_key(&upper), "{upper:?}");
            assert_eq!(canonical, project_key(&lower), "{lower:?}");
        }
        #[cfg(unix)]
        {
            let link = dir.with_file_name(format!(
                "{}-link",
                dir.file_name().unwrap().to_string_lossy()
            ));
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            assert_eq!(canonical, project_key(&link.join("tsconfig.json")));
        }
        // A dotted spelling of the same dir too.
        let dotted = dir.join(".").join("tsconfig.json");
        assert_eq!(canonical, project_key(&dotted));
        // A different project stays different, even as a symlink target's twin.
        let other = scratch("key-other");
        std::fs::write(other.join("tsconfig.json"), "{}").unwrap();
        assert_ne!(canonical, project_key(&other.join("tsconfig.json")));
    }

    #[test]
    fn cache_root_never_falls_back_to_the_cwd() {
        let temp = PathBuf::from("/tmpdir");
        let bare = CacheEnv {
            override_dir: None,
            local_app_data: None,
            xdg_cache_home: None,
            home: None,
            temp_dir: temp.clone(),
        };
        assert_eq!(resolve_cache_root(&bare), temp.join("oam"));

        let overridden = CacheEnv {
            override_dir: Some(PathBuf::from("/explicit")),
            local_app_data: Some(PathBuf::from("/lad")),
            xdg_cache_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
            temp_dir: temp.clone(),
        };
        assert_eq!(resolve_cache_root(&overridden), PathBuf::from("/explicit"));

        let platform = CacheEnv {
            override_dir: None,
            local_app_data: Some(PathBuf::from("/lad")),
            xdg_cache_home: Some(PathBuf::from("/xdg")),
            home: Some(PathBuf::from("/home/u")),
            temp_dir: temp.clone(),
        };
        let expected = if cfg!(windows) {
            PathBuf::from("/lad").join("oam")
        } else {
            PathBuf::from("/xdg").join("oam")
        };
        assert_eq!(resolve_cache_root(&platform), expected);

        let home_only = CacheEnv {
            override_dir: None,
            local_app_data: None,
            xdg_cache_home: None,
            home: Some(PathBuf::from("/home/u")),
            temp_dir: temp.clone(),
        };
        let expected = if cfg!(windows) {
            temp.join("oam")
        } else {
            PathBuf::from("/home/u").join(".cache").join("oam")
        };
        assert_eq!(resolve_cache_root(&home_only), expected);
    }

    #[test]
    fn sidecar_paths_hang_off_the_state_file() {
        let state = Path::new("/c/ts-daemons/abc.json");
        assert_eq!(
            sidecar(state, "error"),
            PathBuf::from("/c/ts-daemons/abc.json.error")
        );
        assert_eq!(
            sidecar(state, "spawn-failed"),
            PathBuf::from("/c/ts-daemons/abc.json.spawn-failed")
        );
    }

    #[test]
    fn spawn_failure_marker_round_trips() {
        let dir = scratch("marker");
        let state = dir.join("x.json");
        assert!(spawn_failure_age(&state).is_none());
        mark_spawn_failed(&state);
        let age = spawn_failure_age(&state).expect("marker readable");
        assert!(age < Duration::from_secs(5), "{age:?}");
    }
}
