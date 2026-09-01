//! oam_ts: TypeScript type checking via tsgo (the TypeScript 7 Go-native
//! compiler) — the typed half of oam's wedge. Bun executes types-blind;
//! Node strips without checking; `oam check` turns tsgo's output into ODIF
//! so humans get pretty diagnostics and agents get stable codes + spans
//! from the same stream.
//!
//! Shape: one `tsgo --pretty false --noEmit` invocation parsed from the
//! classic tsc machine format (`path(line,col): severity TSnnnn: msg`,
//! CWD-relative paths — both verified against tsgo 7.0.0-dev), either
//! one-shot or through the warm per-project daemon in [`daemon`] (which
//! caches by tree fingerprint and runs the same invocation). Every tsgo run
//! is bounded by `OAM_TSGO_TIMEOUT_MS` (default 300s) and cancellable via
//! [`TsgoHandle`]; the @typescript/api in-process checker slots in behind
//! the same CLI surface and ODIF mapping when it matures.
//!
//! Stable codes this crate emits for its OWN failures (tsgo's TSnnnn codes
//! pass through as OAM-TSnnnn): 0000 tsgo missing, 0002 could not invoke /
//! unexpected version, 0003 no tsconfig for a directory target, 0004 tsgo
//! exited without diagnostics, 0006 tsgo timed out, 0007 run cancelled.
//! (0005 — check did not finish before the program exited — is emitted by
//! `oam run`'s warn mode in oam_cli.)

// Diagnostic-as-error is ~200 bytes on cold failure paths; boxing would tax
// every caller's API for nothing. Same deliberate stance as oam_loader.
#![allow(clippy::result_large_err)]

use oam_diagnostics::{Diagnostic, Origin, Position, Severity, Span};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub mod daemon;

/// tsgo not found / not runnable. Stable code so tooling (and our own e2e
/// skip logic) can detect the condition.
const TSGO_MISSING: &str = "OAM-TS0000";

fn ts_error(code: &str, message: String) -> Diagnostic {
    Diagnostic::new(code, Severity::Error, Origin::Typecheck, message)
}

fn missing_tsgo(detail: &str) -> Diagnostic {
    ts_error(
        TSGO_MISSING,
        format!(
            "tsgo (TypeScript native compiler) not found: {detail}. Install with \
             `npm i -g @typescript/native-preview` (or as a devDependency) or set \
             OAM_TSGO to the binary path"
        ),
    )
}

/// Wall-clock bound on a single tsgo run (`OAM_TSGO_TIMEOUT_MS`, default
/// 300s — large monorepos legitimately run for minutes; the point is that a
/// wedged tsgo surfaces as a diagnostic instead of hanging every caller).
pub fn tsgo_timeout() -> Duration {
    std::env::var("OAM_TSGO_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(300))
}

// ------------------------------------------------------------ cancellation --

#[derive(Default)]
enum HandleState {
    #[default]
    Idle,
    Running(u32),
    Cancelled,
}

/// Cancellation handle for the tsgo child of one check. Clone it across
/// threads: `cancel()` kills the in-flight tsgo (whole process tree) and
/// makes any LATER spawn through the same handle refuse to start, so a
/// caller that gives up on a check (warn-mode `oam run` exiting, the daemon
/// shutting down) never leaves a type-check burning CPU behind it.
#[derive(Clone, Default)]
pub struct TsgoHandle(Arc<Mutex<HandleState>>);

impl TsgoHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Kill the in-flight tsgo, if any, and refuse future spawns. Returns
    /// whether a child was actually killed.
    pub fn cancel(&self) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let killed = if let HandleState::Running(pid) = *state {
            kill_tree(pid);
            true
        } else {
            false
        };
        *state = HandleState::Cancelled;
        killed
    }

    /// Record a spawned child. false = already cancelled; the caller must
    /// kill what it just spawned.
    fn register(&self, pid: u32) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*state, HandleState::Cancelled) {
            return false;
        }
        *state = HandleState::Running(pid);
        true
    }

    fn clear(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(*state, HandleState::Running(_)) {
            *state = HandleState::Idle;
        }
    }

    fn is_cancelled(&self) -> bool {
        matches!(
            *self.0.lock().unwrap_or_else(|p| p.into_inner()),
            HandleState::Cancelled
        )
    }
}

/// Kill `pid` and its descendants with the OS's own tools (no FFI, no libc
/// dependency — oam_ts stays a plain crate). The tree matters: the npm shim
/// makes the real compiler a grandchild (cmd.exe -> node -> tsgo.exe on
/// Windows; node -> tsgo on Unix when `process.execve` is unavailable), so
/// killing only the direct child would leave the actual work running.
/// Best-effort: a missing `taskkill`/`pkill` degrades to the old behaviour
/// (the child finishes on its own).
fn kill_tree(pid: u32) {
    let pid = pid.to_string();
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        // Children first (they are still findable by parent pid), then the
        // child itself.
        let _ = Command::new("pkill")
            .args(["-KILL", "-P", &pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("kill")
            .args(["-KILL", &pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

enum RunFailure {
    Spawn(std::io::Error),
    TimedOut,
    Cancelled,
}

/// Spawn `command` with piped output and collect it, killing the whole
/// process tree at `timeout`. The child is registered on `handle` so another
/// thread can cancel it mid-run. A dedicated waiter thread owns the `Child`
/// (std's `wait_with_output` drains both pipes without deadlocking); on
/// timeout/cancel that thread is simply abandoned — it returns as soon as
/// the killed tree closes its pipe ends.
fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    handle: &TsgoHandle,
) -> Result<Output, RunFailure> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(RunFailure::Spawn)?;
    let pid = child.id();
    if !handle.register(pid) {
        kill_tree(pid);
        std::thread::spawn(move || {
            let _ = child.wait_with_output();
        });
        return Err(RunFailure::Cancelled);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let result = match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if handle.is_cancelled() {
                Err(RunFailure::Cancelled)
            } else {
                Ok(output)
            }
        }
        Ok(Err(e)) => Err(RunFailure::Spawn(e)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            kill_tree(pid);
            Err(RunFailure::TimedOut)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(RunFailure::Spawn(
            std::io::Error::other("tsgo waiter thread exited without a result"),
        )),
    };
    handle.clear();
    result
}

// --------------------------------------------------------------- discovery --

/// tsgo program candidates, in resolution order: an explicit `OAM_TSGO`
/// wins outright (an override that silently lost to a project-local shim
/// would be a debugging trap); else the nearest `node_modules/.bin/tsgo`
/// walking up from `from` (a pinned `@typescript/native-preview`
/// devDependency beats whatever is on PATH); else the bare PATH names.
///
/// On Windows the npm shim is tsgo.cmd — spawned DIRECTLY (not via cmd /C):
/// Rust >=1.77 spawns .cmd with batch-aware quoting (errors on
/// unrepresentable args instead of silently misquoting — cmd /C expanded
/// %VAR% inside quoted paths, review finding) and yields a real NotFound
/// io::Error when the shim is absent (cmd /C always spawned, turning
/// missing-tsgo into a 9009 misclassified as OAM-TS0004, review finding).
fn tsgo_candidates(from: &Path) -> Result<Vec<PathBuf>, Diagnostic> {
    if let Ok(explicit) = std::env::var("OAM_TSGO") {
        let path = PathBuf::from(&explicit);
        if !path.is_file() {
            return Err(missing_tsgo(&format!(
                "OAM_TSGO points to {explicit}, which does not exist"
            )));
        }
        return Ok(vec![path]);
    }
    let mut candidates = Vec::new();
    if let Some(local) = project_local_tsgo(from) {
        candidates.push(local);
    }
    if cfg!(windows) {
        candidates.push(PathBuf::from("tsgo.cmd"));
        candidates.push(PathBuf::from("tsgo.exe"));
    } else {
        candidates.push(PathBuf::from("tsgo"));
    }
    Ok(candidates)
}

/// Nearest `node_modules/.bin/tsgo` upward from `from` (a file or dir).
fn project_local_tsgo(from: &Path) -> Option<PathBuf> {
    let mut dir = if from.is_dir() {
        from.to_path_buf()
    } else {
        from.parent()?.to_path_buf()
    };
    let names: &[&str] = if cfg!(windows) {
        &["tsgo.cmd", "tsgo.exe"]
    } else {
        &["tsgo"]
    };
    loop {
        let bin = dir.join("node_modules").join(".bin");
        for name in names {
            let candidate = bin.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The program `from` would resolve to, as a string, WITHOUT probing it —
/// the daemon records this at spawn and clients compare it per check, so a
/// later `OAM_TSGO=...` or a newly installed project-local tsgo retires the
/// old daemon instead of being silently ignored for its whole idle life.
pub(crate) fn tsgo_lookup(from: &Path) -> String {
    tsgo_candidates(from)
        .ok()
        .and_then(|c| c.into_iter().next())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `Version 7.0.0-dev...` — the shape tsgo prints for `--version`. Accepts
/// any major >= 7 so a future TypeScript 8 native compiler is not rejected;
/// rejects tsc 5.x (a different binary named tsgo) and anything that is not
/// a compiler at all.
fn tsgo_version_ok(version: &str) -> bool {
    let Some(rest) = version.trim().strip_prefix("Version ") else {
        return false;
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let Ok(major) = digits.parse::<u32>() else {
        return false;
    };
    major >= 7 && rest[digits.len()..].starts_with('.')
}

fn probed_versions() -> &'static Mutex<HashMap<String, String>> {
    static PROBED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    PROBED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a bare program name against PATH (a candidate with any directory
/// component is returned as-is). The names we search already carry their
/// Windows extension (`tsgo.cmd` / `tsgo.exe`), so no PATHEXT logic is
/// needed. None = not found on PATH (the spawn's own search may still find
/// it, e.g. next to the exe; that case just skips the disk cache).
fn resolve_on_path(name: &Path) -> Option<PathBuf> {
    if name.components().count() > 1 {
        return Some(name.to_path_buf());
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A binary's identity for the version-probe cache: path + mtime + len.
/// None = cannot stat (then the probe is simply not cacheable on disk).
fn binary_identity(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}|{mtime}|{}", path.to_string_lossy(), meta.len()))
}

fn version_cache_path() -> PathBuf {
    daemon::cache_root().join("tsgo-versions.json")
}

fn load_version_cache(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Best-effort (it is a cache): last writer wins, corrupt files read as
/// empty, and the map is capped so it cannot grow without bound.
fn store_version_cache(path: &Path, mut map: HashMap<String, String>) {
    const CAP: usize = 16;
    while map.len() > CAP {
        let Some(key) = map.keys().next().cloned() else {
            break;
        };
        map.remove(&key);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&map) {
        let _ = std::fs::write(path, json);
    }
}

/// Resolve the tsgo to run for `from` and prove it IS tsgo: probe
/// `--version` and require the `Version <major>.` shape with major >= 7.
/// Without this, any exit-0 program named tsgo — a broken shim, a tsc 5
/// alias — read as a clean check.
///
/// The probe is paid once per BINARY, not per run: successful probes are
/// cached in-process and on disk keyed by the resolved path + mtime + len
/// (the probe itself costs a whole cmd->node->tsgo chain on Windows,
/// measured ~600ms — doubling every one-shot check). An upgraded binary
/// changes mtime/len and re-probes; failures are never cached.
pub(crate) fn resolve_tsgo(
    from: &Path,
    handle: &TsgoHandle,
) -> Result<(PathBuf, String), Diagnostic> {
    let mut last_error: Option<std::io::Error> = None;
    for candidate in tsgo_candidates(from)? {
        let resolved = resolve_on_path(&candidate).unwrap_or_else(|| candidate.clone());
        let identity = binary_identity(&resolved);
        if let Some(identity) = &identity {
            let cached = probed_versions()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(identity)
                .cloned();
            if let Some(version) = cached {
                return Ok((resolved, version));
            }
            if let Some(version) = load_version_cache(&version_cache_path())
                .remove(identity)
                .filter(|v| tsgo_version_ok(v))
            {
                probed_versions()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(identity.clone(), version.clone());
                return Ok((resolved, version));
            }
        }
        let mut command = Command::new(&resolved);
        command.arg("--version");
        match run_with_timeout(command, tsgo_timeout(), handle) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !tsgo_version_ok(&stdout) {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    return Err(ts_error(
                        "OAM-TS0002",
                        format!(
                            "unexpected tsgo version from {}: `--version` printed {:?} (exit {}){}; \
                             expected `Version <major>.<minor>...` with major >= 7 \
                             (@typescript/native-preview)",
                            resolved.display(),
                            stdout,
                            output.status,
                            if stderr.is_empty() {
                                String::new()
                            } else {
                                format!(", stderr: {stderr}")
                            }
                        ),
                    ));
                }
                if let Some(identity) = identity {
                    probed_versions()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .insert(identity.clone(), stdout.clone());
                    let cache_path = version_cache_path();
                    let mut map = load_version_cache(&cache_path);
                    map.insert(identity, stdout.clone());
                    store_version_cache(&cache_path, map);
                }
                return Ok((resolved, stdout));
            }
            Err(RunFailure::Spawn(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                last_error = Some(e);
            }
            Err(RunFailure::Spawn(e)) => {
                return Err(ts_error(
                    "OAM-TS0002",
                    format!("could not invoke {}: {e}", resolved.display()),
                ));
            }
            Err(RunFailure::TimedOut) => return Err(timed_out(&resolved, "--version")),
            Err(RunFailure::Cancelled) => return Err(cancelled()),
        }
    }
    Err(missing_tsgo(&last_error.map_or_else(
        || "not on PATH and no node_modules/.bin/tsgo upward".to_string(),
        |e| e.to_string(),
    )))
}

fn timed_out(program: &Path, what: &str) -> Diagnostic {
    let timeout = tsgo_timeout();
    ts_error(
        "OAM-TS0006",
        format!(
            "tsgo timed out after {}s running `{} {what}` (OAM_TSGO_TIMEOUT_MS={}); the process tree was killed",
            timeout.as_secs(),
            program.display(),
            timeout.as_millis()
        ),
    )
}

fn cancelled() -> Diagnostic {
    ts_error(
        "OAM-TS0007",
        "tsgo run cancelled before it finished".to_string(),
    )
}

/// Run tsgo with `args` from `base`, resolving the binary from `from`.
fn run_tsgo(
    args: &[&OsStr],
    base: &Path,
    from: &Path,
    handle: &TsgoHandle,
) -> Result<Output, Diagnostic> {
    let (program, _version) = resolve_tsgo(from, handle)?;
    let mut command = Command::new(&program);
    command.args(args).current_dir(base);
    match run_with_timeout(command, tsgo_timeout(), handle) {
        Ok(output) => Ok(output),
        Err(RunFailure::Spawn(e)) => Err(ts_error(
            "OAM-TS0002",
            format!("could not invoke {}: {e}", program.display()),
        )),
        Err(RunFailure::TimedOut) => Err(timed_out(
            &program,
            &args
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
        )),
        Err(RunFailure::Cancelled) => Err(cancelled()),
    }
}

/// Is tsgo runnable? Returns its version string. Milliseconds — safe to call
/// from availability probes (a full check() on a project is NOT). Err on a
/// missing binary AND on one that is not a TypeScript 7 compiler.
pub fn available() -> Result<String, Diagnostic> {
    let cwd = std::env::current_dir().map_err(|e| ts_error("OAM-TS0002", format!("cwd: {e}")))?;
    resolve_tsgo(&cwd, &TsgoHandle::new()).map(|(_, version)| version)
}

/// Walk up from `start` looking for tsconfig.json.
///
/// Twin: `oam_loader::find_tsconfig` (crates/oam_loader/src/tsconfig.rs) is
/// the same walk for the runtime resolver, cached per Resolver. This crate
/// must not depend on oam_loader (it would pull oxc and reqwest into the
/// checker), so the two are kept in sync by hand -- change one, change both.
pub fn find_tsconfig(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        let candidate = dir.join("tsconfig.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Type-check `target` (a .ts file or a directory). Returns the diagnostics
/// tsgo produced (empty = clean). Err = could not run the check at all.
pub fn check(target: &Path) -> Result<Vec<Diagnostic>, Diagnostic> {
    check_cancellable(target, &TsgoHandle::new())
}

/// [`check`] whose tsgo child is registered on `handle`, so a caller on
/// another thread can kill it (and get OAM-TS0007 back here).
pub fn check_cancellable(
    target: &Path,
    handle: &TsgoHandle,
) -> Result<Vec<Diagnostic>, Diagnostic> {
    let target = std::path::absolute(target).map_err(|e| {
        ts_error(
            "OAM-TS0002",
            format!("bad check target {}: {e}", target.display()),
        )
    })?;

    let tsconfig = find_tsconfig(&target);
    // Project checks run --incremental with the build-info under oam's own
    // cache (never the user's repo): measured ~1.6x on warm re-checks, and
    // the win grows with project size. Cache-dir failure degrades to a
    // plain non-incremental check, never to an error.
    let build_info = tsconfig.as_ref().and_then(|tsconfig| {
        let dir = daemon::cache_root().join("ts-buildinfo");
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join(format!("{}.tsbuildinfo", daemon::project_key(tsconfig))))
    });
    let common: [&OsStr; 3] = ["--pretty".as_ref(), "false".as_ref(), "--noEmit".as_ref()];
    let (args, base): (Vec<&OsStr>, PathBuf) = match (&tsconfig, target.is_file()) {
        (Some(tsconfig), _) => {
            let base = tsconfig
                .parent()
                .expect("tsconfig has a parent")
                .to_path_buf();
            let mut args = common.to_vec();
            if let Some(build_info) = build_info.as_deref() {
                args.push("--incremental".as_ref());
                args.push("--tsBuildInfoFile".as_ref());
                args.push(build_info.as_os_str());
            }
            args.push("-p".as_ref());
            args.push(tsconfig.as_os_str());
            (args, base)
        }
        (None, true) => {
            let base = target.parent().expect("file has a parent").to_path_buf();
            let mut args = common.to_vec();
            args.push(target.as_os_str());
            (args, base)
        }
        (None, false) => {
            return Err(ts_error(
                "OAM-TS0003",
                format!(
                    "no tsconfig.json found from {} upward; point oam check at a .ts file or add a tsconfig.json",
                    target.display()
                ),
            ));
        }
    };

    // Run from `base` so the CWD-relative paths in tsgo's output join back
    // to absolute ODIF spans unambiguously. The binary resolves from the
    // project dir when there is one (the daemon resolves the same way, so
    // both paths agree on WHICH tsgo checks a project), else the file's dir.
    let output = run_tsgo(&args, &base, &base, handle)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut diagnostics = parse_tsc_output(&stdout, &base);
    diagnostics.extend(parse_tsc_output(&stderr, &base));

    // Non-zero exit with zero parsed diagnostics = tsgo itself failed
    // (bad flags, crashed): surface raw output rather than claiming clean.
    if diagnostics.is_empty() && !output.status.success() {
        return Err(ts_error(
            "OAM-TS0004",
            format!(
                "tsgo exited with {} but produced no diagnostics: {}",
                output.status,
                if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                }
            ),
        ));
    }
    Ok(diagnostics)
}

/// The exact file set tsgo's program for `tsconfig` reads (`--listFilesOnly`:
/// sources, `allowJs` .js, `resolveJsonModule` .json, `@types`, the lib
/// .d.ts files that ship with tsgo, `extends`/`include` targets outside the
/// tsconfig dir). The daemon fingerprints this set, so its cache is
/// invalidated by exactly the edits that can change the diagnostics.
pub(crate) fn list_files(tsconfig: &Path, handle: &TsgoHandle) -> Result<Vec<PathBuf>, Diagnostic> {
    let base = tsconfig
        .parent()
        .ok_or_else(|| ts_error("OAM-TS0002", "tsconfig has no parent".to_string()))?;
    // --noEmit matters: without it a checkJs project fails TS5055 ("would
    // overwrite input file") before listing anything (probed).
    let args: [&OsStr; 4] = [
        "--listFilesOnly".as_ref(),
        "--noEmit".as_ref(),
        "-p".as_ref(),
        tsconfig.as_os_str(),
    ];
    let output = run_tsgo(&args, base, base, handle)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<PathBuf> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && Path::new(line).is_absolute())
        .map(PathBuf::from)
        .collect();
    if files.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ts_error(
            "OAM-TS0004",
            format!(
                "tsgo --listFilesOnly ({}) produced no file list: {}",
                output.status,
                if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                }
            ),
        ));
    }
    Ok(files)
}

/// Fold `.`/`..` and re-join with the platform separator. tsgo prints
/// CWD-relative paths with forward slashes (and `..` segments when the CWD
/// sits below the file), which `Path::join` appends VERBATIM — so a Windows
/// span read `C:\proj\src/a.ts` and any consumer matching spans against the
/// files it edited had to normalize itself. Purely lexical (no
/// canonicalize): symlinks are left alone so the span names the path the
/// user actually has open.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Parse the classic tsc machine format. Lines that don't match the shape
/// are continuation/elaboration lines and append to the previous diagnostic.
fn parse_tsc_output(output: &str, base: &Path) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_tsc_line(line, base) {
            Some(diagnostic) => diagnostics.push(diagnostic),
            None => {
                if let Some(last) = diagnostics.last_mut() {
                    last.message.push('\n');
                    last.message.push_str(line.trim_end());
                }
            }
        }
    }
    diagnostics
}

/// `path(line,col): severity TSnnnn: message`
///
/// Spans are zero-width (end == start): the plain format carries only the
/// start position, and inventing a length would mislead agents that apply
/// edits by span. A tsgo JSON output mode (or `--pretty` parsing) is the
/// way to recover lengths later.
fn parse_tsc_line(line: &str, base: &Path) -> Option<Diagnostic> {
    let (location, rest) = line.split_once("): ")?;
    let (file, position) = location.rsplit_once('(')?;
    let (line_str, col_str) = position.split_once(',')?;
    let line_no: u32 = line_str.trim().parse().ok()?;
    let col_no: u32 = col_str.trim().parse().ok()?;

    let (severity_word, rest) = rest.split_once(' ')?;
    let severity = match severity_word {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => Severity::Info,
    };
    let (ts_code, message) = rest.split_once(": ")?;
    let code_number = ts_code.strip_prefix("TS")?;
    code_number.parse::<u32>().ok()?;

    let file = normalize_path(&base.join(file));
    let position = Position {
        line: line_no,
        col: col_no,
    };
    Some(
        Diagnostic::new(
            format!("OAM-TS{code_number}"),
            severity,
            Origin::Typecheck,
            message,
        )
        .with_span(Span {
            file: file.to_string_lossy().into_owned(),
            start: position.clone(),
            end: position,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_error_lines_with_spans() {
        let out = "src/a.ts(12,8): error TS2345: Argument of type 'number' is not assignable.\n";
        let diags = parse_tsc_output(out, Path::new("/proj"));
        assert_eq!(diags.len(), 1);
        let d = &diags[0];
        assert_eq!(d.code, "OAM-TS2345");
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.origin, Origin::Typecheck);
        assert_eq!(d.spans[0].start, Position { line: 12, col: 8 });
        assert!(d.spans[0].file.ends_with("a.ts"));
    }

    #[test]
    fn continuation_lines_append_to_previous_message() {
        let out = "src/a.ts(1,1): error TS2322: Type 'A' is not assignable to type 'B'.\n  Property 'x' is missing in type 'A'.\n";
        let diags = parse_tsc_output(out, Path::new("/proj"));
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Property 'x' is missing"));
    }

    #[test]
    fn parses_multiple_diagnostics_and_severities() {
        let out = "a.ts(1,1): error TS1000: one.\nb.ts(2,3): warning TS2000: two.\n";
        let diags = parse_tsc_output(out, Path::new("/p"));
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[1].severity, Severity::Warning);
        assert_eq!(diags[1].code, "OAM-TS2000");
    }

    #[test]
    fn garbage_lines_without_prior_diagnostic_are_dropped() {
        let out = "Compiling project...\nnot a diagnostic\n";
        let diags = parse_tsc_output(out, Path::new("/p"));
        assert!(diags.is_empty());
    }

    #[test]
    fn span_paths_are_normalized_to_the_platform_separator() {
        // tsgo's forward-slash, `..`-bearing relative path joined onto a
        // native base must come out as one clean native path.
        let base = if cfg!(windows) {
            Path::new("C:\\proj\\sub")
        } else {
            Path::new("/proj/sub")
        };
        let out = "../src/./a.ts(1,1): error TS1000: x.\n";
        let diags = parse_tsc_output(out, base);
        let expected = if cfg!(windows) {
            "C:\\proj\\src\\a.ts"
        } else {
            "/proj/src/a.ts"
        };
        assert_eq!(diags[0].spans[0].file, expected);
        assert_eq!(
            diags[0].spans[0].start, diags[0].spans[0].end,
            "zero-width by design"
        );
    }

    #[test]
    fn normalize_path_folds_dots_and_stops_at_the_root() {
        let p = normalize_path(Path::new("/a/b/../../../c/./d"));
        assert_eq!(p, PathBuf::from("/c/d"));
        let rel = normalize_path(Path::new("../x/../../y"));
        assert_eq!(rel, PathBuf::from("../../y"));
        #[cfg(windows)]
        {
            let w = normalize_path(Path::new("C:\\proj\\src/../lib/a.ts"));
            assert_eq!(w, PathBuf::from("C:\\proj\\lib\\a.ts"));
        }
    }

    #[test]
    fn version_gate_accepts_tsgo_7_and_rejects_impostors() {
        assert!(tsgo_version_ok("Version 7.0.0-dev.20260610.1"));
        assert!(tsgo_version_ok("Version 8.1.0\n"));
        assert!(!tsgo_version_ok("Version 5.9.2"), "tsc 5 named tsgo");
        assert!(!tsgo_version_ok("junk"));
        assert!(!tsgo_version_ok(""));
        assert!(!tsgo_version_ok("Version 7"), "needs a minor");
        assert!(!tsgo_version_ok("7.0.0"));
    }

    #[test]
    fn version_probe_disk_cache_round_trips_and_keys_on_content() {
        let dir = std::env::temp_dir().join(format!(
            "oam-vercache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("tsgo-versions.json");
        assert!(
            load_version_cache(&cache).is_empty(),
            "missing file = empty"
        );

        let binary = dir.join("tsgo.exe");
        std::fs::write(&binary, "one").unwrap();
        let id_one = binary_identity(&binary).expect("statable");
        let mut map = HashMap::new();
        map.insert(id_one.clone(), "Version 7.0.0-dev".to_string());
        store_version_cache(&cache, map);
        assert_eq!(
            load_version_cache(&cache).get(&id_one).map(String::as_str),
            Some("Version 7.0.0-dev")
        );

        // A replaced binary (different length) gets a different identity,
        // so a stale probe result can never be served for it.
        std::fs::write(&binary, "two-longer").unwrap();
        let id_two = binary_identity(&binary).expect("statable");
        assert_ne!(id_one, id_two);
        assert!(!load_version_cache(&cache).contains_key(&id_two));

        // Corrupt cache reads as empty rather than erroring.
        std::fs::write(&cache, "not json").unwrap();
        assert!(load_version_cache(&cache).is_empty());
    }

    #[test]
    fn cancel_before_spawn_refuses_to_run_and_reports_ts0007() {
        let handle = TsgoHandle::new();
        assert!(!handle.cancel(), "nothing running yet");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("--version");
        match run_with_timeout(command, Duration::from_secs(5), &handle) {
            Err(RunFailure::Cancelled) => {}
            Err(RunFailure::Spawn(e)) => panic!("spawn failed: {e}"),
            Err(RunFailure::TimedOut) => panic!("timed out"),
            Ok(_) => panic!("a cancelled handle must not run anything"),
        }
    }

    #[test]
    fn timeout_kills_the_child_and_reports_timed_out() {
        // The test binary itself, run as a sleeper: cargo's harness accepts
        // `--list` instantly, so make it block on stdin instead... stdin is
        // null here, so use a genuinely slow program: ping on Windows,
        // sleep on Unix.
        let mut command = if cfg!(windows) {
            let mut c = Command::new("ping");
            c.args(["-n", "31", "127.0.0.1"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        command.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(root) = std::env::var_os("SYSTEMROOT") {
            command.env("SYSTEMROOT", root);
        }
        let handle = TsgoHandle::new();
        let started = std::time::Instant::now();
        match run_with_timeout(command, Duration::from_millis(300), &handle) {
            Err(RunFailure::TimedOut) => {}
            Err(RunFailure::Spawn(e)) => {
                eprintln!("skipping: sleeper unavailable: {e}");
                return;
            }
            Err(RunFailure::Cancelled) => panic!("not cancelled"),
            Ok(_) => panic!("must time out"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout must not wait for the natural exit"
        );
    }
}
