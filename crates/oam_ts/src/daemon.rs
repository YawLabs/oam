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
//! one-shot. Every client-side failure (spawn, connect, version skew,
//! protocol) degrades silently to the one-shot path.
//!
//! Protocol: one JSONL request line in, one response line out, connection
//! per request. The token (in a user-only state file) authenticates that
//! the thing on the recorded port is OUR daemon — loopback port numbers
//! get reused by other software.

use crate::{Diagnostic, find_tsconfig};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SPAWN_WAIT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(120);

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
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
enum Request {
    Ping { token: String },
    Check { token: String },
    Shutdown { token: String },
}

#[derive(Serialize, Deserialize)]
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
}

/// Cache/state root: OAM_CACHE_DIR override (tests), else the platform dir.
pub(crate) fn cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("OAM_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("oam")
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cache")
            });
        base.join("oam")
    }
}

pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Stable per-project key from the canonicalized tsconfig path.
pub(crate) fn project_key(tsconfig: &Path) -> String {
    let canonical = std::path::absolute(tsconfig).unwrap_or_else(|_| tsconfig.to_path_buf());
    format!("{:016x}", fnv1a64(canonical.to_string_lossy().as_bytes()))
}

fn state_path(tsconfig: &Path) -> PathBuf {
    cache_root()
        .join("ts-daemons")
        .join(format!("{}.json", project_key(tsconfig)))
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

/// Cheap whole-project content fingerprint: (rel path, mtime, size) of every
/// TS source + tsconfig, FNV-mixed. None = too big to fingerprint (then we
/// simply never serve from cache — correctness first).
fn fingerprint(root: &Path) -> Option<u64> {
    const CAP: usize = 50_000;
    let mut entries: Vec<(String, u128, u64)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = std::fs::read_dir(&dir).ok()?;
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !matches!(name.as_ref(), "node_modules" | ".git" | "target" | ".oam") {
                    stack.push(path);
                }
                continue;
            }
            let relevant = name == "tsconfig.json"
                || std::path::Path::new(name.as_ref())
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "ts" | "tsx" | "mts" | "cts"));
            if !relevant {
                continue;
            }
            let meta = entry.metadata().ok()?;
            let mtime = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            entries.push((rel, mtime, meta.len()));
            if entries.len() > CAP {
                return None;
            }
        }
    }
    entries.sort();
    let mut hash: u64 = 0xcbf29ce484222325;
    for (rel, mtime, len) in entries {
        hash ^= fnv1a64(rel.as_bytes()) ^ (mtime as u64) ^ len.rotate_left(32);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(hash)
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
}

fn read_state(tsconfig: &Path) -> Option<StateFile> {
    let raw = std::fs::read_to_string(state_path(tsconfig)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn request(state: &StateFile, request: &Request) -> Option<Response> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], state.port));
    let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    let mut writer = stream.try_clone().ok()?;
    writeln!(writer, "{}", serde_json::to_string(request).ok()?).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// Check `target`'s project through the daemon, spawning it if needed.
/// Err = use the one-shot path instead (NEVER a user-facing failure).
pub fn check_via_daemon(target: &Path) -> Result<Vec<Diagnostic>, String> {
    let target = std::path::absolute(target).map_err(|e| e.to_string())?;
    let tsconfig = find_tsconfig(&target).ok_or("no tsconfig: single-file checks stay one-shot")?;

    if let Some(diagnostics) = try_existing(&tsconfig) {
        return Ok(diagnostics);
    }
    spawn_daemon(&tsconfig)?;
    let deadline = Instant::now() + SPAWN_WAIT;
    while Instant::now() < deadline {
        if let Some(diagnostics) = try_existing(&tsconfig) {
            return Ok(diagnostics);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("daemon did not come up in time".into())
}

fn try_existing(tsconfig: &Path) -> Option<Vec<Diagnostic>> {
    let state = read_state(tsconfig)?;
    if state.version != env!("CARGO_PKG_VERSION") {
        // Version skew: retire the old daemon, caller respawns.
        let _ = request(
            &state,
            &Request::Shutdown {
                token: state.token.clone(),
            },
        );
        let _ = std::fs::remove_file(state_path(tsconfig));
        return None;
    }
    let response = request(
        &state,
        &Request::Check {
            token: state.token.clone(),
        },
    )?;
    if response.ok {
        response.diagnostics
    } else {
        None
    }
}

pub fn status(target: &Path) -> DaemonStatus {
    let down = DaemonStatus {
        running: false,
        pid: None,
        port: None,
        version: None,
        checks_served: None,
        cache_hits: None,
    };
    let Ok(target) = std::path::absolute(target) else {
        return down;
    };
    let Some(tsconfig) = find_tsconfig(&target) else {
        return down;
    };
    let Some(state) = read_state(&tsconfig) else {
        return down;
    };
    match request(
        &state,
        &Request::Ping {
            token: state.token.clone(),
        },
    ) {
        Some(response) if response.ok => DaemonStatus {
            running: true,
            pid: Some(state.pid),
            port: Some(state.port),
            version: response.version,
            checks_served: response.checks_served,
            cache_hits: response.cache_hits,
        },
        _ => down,
    }
}

/// Stop the project's daemon if one is running. Returns whether one was.
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
    )
    .is_some_and(|r| r.ok);
    let _ = std::fs::remove_file(state_path(&tsconfig));
    stopped
}

/// Windows: a spawned child inherits EVERY inheritable handle in our table,
/// not just its configured stdio. When `oam check` itself runs with piped
/// stdio (cargo test, CI, agents), those pipe write-handles leak into the
/// detached daemon, and the parent's `.output()` blocks until the DAEMON
/// exits — observed as a 30-minute test hang. Clearing the inherit flag on
/// our own std handles before the spawn closes the leak; our own IO is
/// unaffected (the flag only governs inheritance).
#[cfg(windows)]
fn unshare_std_handles() {
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

fn spawn_daemon(tsconfig: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("__oamd-ts")
        .arg(tsconfig)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Propagate test/ops overrides so the daemon lives in the same world.
    for var in ["OAM_CACHE_DIR", "OAM_DAEMON_IDLE_MS", "OAM_TSGO"] {
        if let Ok(value) = std::env::var(var) {
            command.env(var, value);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        unshare_std_handles();
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    command.spawn().map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------- server --

/// Daemon main loop. Runs until shutdown request or idle timeout; the state
/// file is written only once the listener is live (clients poll for it).
pub fn serve(tsconfig: &Path) -> std::io::Result<()> {
    let tsconfig = std::path::absolute(tsconfig)?;
    let root = tsconfig
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let token = random_token();

    let state = StateFile {
        pid: std::process::id(),
        port,
        token: token.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        tsconfig: tsconfig.to_string_lossy().into_owned(),
    };
    let state_file = state_path(&tsconfig);
    std::fs::create_dir_all(state_file.parent().expect("state dir"))?;
    std::fs::write(&state_file, serde_json::to_string(&state)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o600));
    }

    let mut last_activity = Instant::now();
    let mut checks_served: u64 = 0;
    let mut cache_hits: u64 = 0;
    let mut cache: Option<(u64, Vec<Diagnostic>)> = None;

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                last_activity = Instant::now();
                let keep_running = handle_connection(
                    stream,
                    &token,
                    &tsconfig,
                    &root,
                    &mut cache,
                    &mut checks_served,
                    &mut cache_hits,
                );
                if !keep_running {
                    let _ = std::fs::remove_file(&state_file);
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last_activity.elapsed() > idle_shutdown() {
                    let _ = std::fs::remove_file(&state_file);
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Returns false when the daemon should exit (shutdown request).
fn handle_connection(
    stream: TcpStream,
    token: &str,
    tsconfig: &Path,
    root: &Path,
    cache: &mut Option<(u64, Vec<Diagnostic>)>,
    checks_served: &mut u64,
    cache_hits: &mut u64,
) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_nonblocking(false);
    let Ok(mut writer) = stream.try_clone() else {
        return true;
    };
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return true;
    }
    let respond = |writer: &mut TcpStream, response: &Response| {
        if let Ok(json) = serde_json::to_string(response) {
            let _ = writeln!(writer, "{json}");
        }
    };
    let fail = |writer: &mut TcpStream, message: &str| {
        respond(
            writer,
            &Response {
                ok: false,
                error: Some(message.to_string()),
                cached: None,
                diagnostics: None,
                version: None,
                checks_served: None,
                cache_hits: None,
            },
        );
    };

    let Ok(request) = serde_json::from_str::<Request>(line.trim()) else {
        fail(&mut writer, "malformed request");
        return true;
    };
    let presented = match &request {
        Request::Ping { token } | Request::Check { token } | Request::Shutdown { token } => token,
    };
    if presented != token {
        fail(&mut writer, "bad token");
        return true;
    }

    match request {
        Request::Ping { .. } => {
            respond(
                &mut writer,
                &Response {
                    ok: true,
                    error: None,
                    cached: None,
                    diagnostics: None,
                    version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    checks_served: Some(*checks_served),
                    cache_hits: Some(*cache_hits),
                },
            );
            true
        }
        Request::Shutdown { .. } => {
            respond(
                &mut writer,
                &Response {
                    ok: true,
                    error: None,
                    cached: None,
                    diagnostics: None,
                    version: None,
                    checks_served: None,
                    cache_hits: None,
                },
            );
            false
        }
        Request::Check { .. } => {
            *checks_served += 1;
            let fp = fingerprint(root);
            if let (Some(fp), Some((cached_fp, diagnostics))) = (fp, cache.as_ref())
                && fp == *cached_fp
            {
                *cache_hits += 1;
                respond(
                    &mut writer,
                    &Response {
                        ok: true,
                        error: None,
                        cached: Some(true),
                        diagnostics: Some(diagnostics.clone()),
                        version: None,
                        checks_served: None,
                        cache_hits: None,
                    },
                );
                return true;
            }
            match crate::check(tsconfig) {
                Ok(diagnostics) => {
                    if let Some(fp) = fp {
                        *cache = Some((fp, diagnostics.clone()));
                    }
                    respond(
                        &mut writer,
                        &Response {
                            ok: true,
                            error: None,
                            cached: Some(false),
                            diagnostics: Some(diagnostics),
                            version: None,
                            checks_served: None,
                            cache_hits: None,
                        },
                    );
                }
                Err(failure) => fail(&mut writer, &failure.to_jsonl()),
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b"oam"), fnv1a64(b"oam"));
        assert_ne!(fnv1a64(b"oam"), fnv1a64(b"oan"));
    }

    #[test]
    fn fingerprint_changes_on_edit() {
        let dir = std::env::temp_dir().join(format!("oam-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.ts"), "export const a = 1;").unwrap();
        let before = fingerprint(&dir).expect("fingerprints");
        // Size change guarantees a new fingerprint even on coarse mtimes.
        std::fs::write(dir.join("a.ts"), "export const a = 12;").unwrap();
        let after = fingerprint(&dir).expect("fingerprints");
        assert_ne!(before, after);
        // Irrelevant files don't participate.
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        assert_eq!(after, fingerprint(&dir).unwrap());
    }

    #[test]
    fn project_keys_are_path_stable() {
        let a = project_key(Path::new("/p/tsconfig.json"));
        assert_eq!(a, project_key(Path::new("/p/tsconfig.json")));
        assert_ne!(a, project_key(Path::new("/q/tsconfig.json")));
    }
}
