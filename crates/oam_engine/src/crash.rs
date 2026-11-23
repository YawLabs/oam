//! Internal-grade crash reporter (NOT public telemetry).
//!
//! Today a Rust panic or a V8 OOM aborts raw, with no context. This module
//! gives both paths a structured stderr banner + a crash file under the oam
//! cache dir, matching the reliability bar set by Bun's crash banner.
//!
//! Two entry points, one reporter:
//!   * [`install_panic_hook`] -- a `std::panic::set_hook` installed first thing
//!     in `main()`. It only PRINTS + records; it never aborts. This is
//!     deliberate: the `catch_unwind` boundary in oam_core (op-future panics)
//!     must still recover. A print-only hook does not interfere with
//!     unwinding, so recoverable panics stay recoverable; only an unwind that
//!     reaches a real boundary (out of `main`, or a thread top) ends the
//!     process, exactly as before.
//!   * [`v8_oom_handler`] -- registered via `Isolate::set_oom_error_handler`
//!     at every isolate creation. A V8 heap/process OOM is terminal (V8 cannot
//!     continue), so this variant prints the banner then `abort()`s -- Node's
//!     "FATAL ERROR: ... JavaScript heap out of memory" behavior.
//!
//! Dependency-free by design: no minidump/backtrace crates (that is the
//! public-flip line). Backtraces come from std's `RUST_BACKTRACE`-gated
//! `std::backtrace::Backtrace`; in `--release` (`strip = "symbols"`) frames
//! are addresses without names -- acceptable for internal triage.

use oam_diagnostics::own_files::{self, Audience};
use std::backtrace::{Backtrace, BacktraceStatus};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static HOOK_INSTALLED: Once = Once::new();

/// Short git SHA embedded at build time, if a build script exposes it. Absent
/// on a plain `cargo build` / tarball -> the banner shows "unknown". Wiring it
/// is a one-line `cargo:rustc-env=OAM_GIT_SHA=...` in a build.rs (left out here
/// to avoid git-dir rebuild churn; the version alone identifies a release).
const GIT_SHA: Option<&str> = option_env!("OAM_GIT_SHA");

/// Cache/state root, mirroring `oam_ts::daemon::cache_root` (duplicated so the
/// reporter stays self-contained and callable mid-crash with no cross-crate
/// state): `OAM_CACHE_DIR` override, else the per-platform user cache dir,
/// else `<temp>/oam` -- never the process cwd. An empty variable counts as
/// unset, as it does in the copies this mirrors (`code_cache::CacheEnv`,
/// `oam_loader::transpile_cache`): a scrubbed environment must not put crash
/// files into whatever directory oam was run from.
///
/// Resolved once, when [`install_panic_hook`] runs at start-up (or at the
/// first report, if the hook was never installed): a relative
/// `OAM_CACHE_DIR` is made absolute against the start-up working directory,
/// as `code_cache` and the daemon make theirs, so a later `process.chdir()`
/// cannot move where reports go. None: the root is the temp-dir fallback
/// and that `<temp>/oam` is not this user's own
/// ([`crate::code_cache::temp_base_is_own`]), so no file is written. The
/// flag says the root is that fallback, which [`write_crash_file`] creates
/// (or judges again) when a report is written: nothing is created here, at
/// start-up.
fn cache_root() -> Option<(&'static Path, bool)> {
    static ROOT: OnceLock<Option<(PathBuf, bool)>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let temp = std::env::temp_dir();
        let root = resolve_cache_root(|key| std::env::var(key).ok(), cfg!(windows), temp.clone());
        let fallback = root == temp.join("oam");
        (!fallback || crate::code_cache::temp_base_is_own(&root)).then_some((root, fallback))
    })
    .as_ref()
    .map(|(root, fallback)| (root.as_path(), *fallback))
}

/// Pure path logic behind [`cache_root`]. `windows` is a parameter rather
/// than `cfg!` so both branches are unit-tested on every host. As in
/// `oam_ts::daemon::CacheEnv`, a relative `OAM_CACHE_DIR` is made absolute
/// and a relative platform variable is ignored.
fn resolve_cache_root(
    lookup: impl Fn(&str) -> Option<String>,
    windows: bool,
    temp_dir: PathBuf,
) -> PathBuf {
    let var = |key: &str| lookup(key).filter(|v| !v.is_empty());
    let absolute_only = |key: &str| var(key).map(PathBuf::from).filter(|dir| dir.is_absolute());
    if let Some(dir) = var("OAM_CACHE_DIR") {
        let dir = PathBuf::from(dir);
        return std::path::absolute(&dir).unwrap_or(dir);
    }
    let platform = if windows {
        absolute_only("LOCALAPPDATA")
    } else {
        absolute_only("XDG_CACHE_HOME").or_else(|| absolute_only("HOME").map(|h| h.join(".cache")))
    };
    platform.unwrap_or(temp_dir).join("oam")
}

/// How many crash files `<cache>/crashes` keeps: after each write the oldest
/// beyond this many are removed, so a long-lived process whose op keeps
/// panicking (each one recovered and reported) cannot grow it without bound.
const CRASH_FILES_KEPT: usize = 50;

/// Milliseconds since the Unix epoch. `SystemTime` is permitted in runtime Rust
/// -- only JS `Date.now` is trapped for record/replay determinism.
fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The shared banner header. `what` is the headline verb (`panicked` for a
/// Rust panic, which the process may recover from; `crashed` for a V8 OOM,
/// which ends it); `kind` is a short crash class; `detail` is the multi-line
/// body (panic message + backtrace, or the OOM specifics).
fn banner(what: &str, kind: &str, detail: &str) -> String {
    let sha = GIT_SHA.unwrap_or("unknown");
    let thread = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string();
    let ver = env!("CARGO_PKG_VERSION");
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let pid = std::process::id();
    format!(
        "\n======== oam {what} ({kind}) ========\n\
         oam v{ver} (git {sha}) on {os}-{arch}, pid {pid}\n\
         thread: {thread}\n\
         {detail}\n"
    )
}

/// Write `body` to `<cache>/crashes/crash-<millis>-<pid>-<n>.txt`,
/// best-effort, then prune the directory to [`CRASH_FILES_KEPT`] files. `n`
/// counts this process's reports, so two in the same millisecond do not
/// overwrite each other. Returns the path when the write lands so the caller
/// can point at it.
fn write_crash_file(body: &str) -> Option<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let (root, fallback) = cache_root()?;
    // The temp-dir fallback's base is created owner-only, or judged again if
    // another process made it since start-up (see `claim_temp_base`).
    if fallback && !crate::code_cache::claim_temp_base(root) {
        return None;
    }
    let dir = root.join("crashes");
    // The report's own modes, not the running program's mask: `own_files`.
    own_files::create_dir_all(&dir, Audience::Kept).ok()?;
    let path = dir.join(format!(
        "crash-{}-{}-{}.txt",
        unix_millis(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = own_files::create(&path, Audience::Kept).ok()?;
    file.write_all(body.as_bytes()).ok()?;
    prune_crash_files(&dir, CRASH_FILES_KEPT);
    Some(path)
}

/// Remove the oldest `crash-*.txt` files in `dir` beyond the newest `keep`,
/// by modification time. Best-effort; nothing else in `dir` is touched.
fn prune_crash_files(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reports: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("crash-") && name.ends_with(".txt")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    if reports.len() <= keep {
        return;
    }
    // Newest first; ties by name, whose millisecond prefix orders them.
    reports.sort_by(|a, b| b.cmp(a));
    for (_, path) in reports.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// Emit `text` to stderr and to a crash file. Shared by both entry points.
/// Rust's stderr lock is re-entrant, so this is safe even when a panic is
/// unwinding through a held stderr guard (e.g. a panic inside `println!`).
fn report(text: &str) {
    let file = write_crash_file(text);
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    match file {
        Some(path) => {
            let _ = writeln!(err, "crash report: {}", path.display());
        }
        None => {
            let _ = err.write_all(b"crash report: (could not write file)\n");
        }
    }
    let _ = err.write_all(b"=======================================\n");
    let _ = err.flush();
}

/// Install the process-wide panic hook. Idempotent. Prints + records only,
/// never aborts -- so op-future panics caught by oam_core's `catch_unwind`
/// still recover.
pub fn install_panic_hook() {
    HOOK_INSTALLED.call_once(|| {
        // Resolve the report directory now, against the start-up cwd.
        let _ = cache_root();
        std::panic::set_hook(Box::new(|info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "unknown location".to_string());
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic payload>");
            let backtrace = Backtrace::capture();
            let bt = match backtrace.status() {
                BacktraceStatus::Captured => format!("backtrace:\n{backtrace}"),
                BacktraceStatus::Disabled => {
                    "backtrace: disabled (set RUST_BACKTRACE=1 to capture)".to_string()
                }
                _ => "backtrace: unavailable".to_string(),
            };
            let detail = format!("panicked at {location}\n  {message}\n{bt}");
            report(&banner("panicked", "panic", &detail));
        }));
    });
}

/// V8 out-of-memory handler, registered per-isolate. A V8 OOM is unrecoverable
/// (the isolate is wedged), so this prints the banner then `abort()`s --
/// mirroring Node's fatal heap-OOM behavior. This is the ONLY crash-reporter
/// path that aborts; the panic hook never does.
///
/// The signature must match `v8::OomErrorCallback` byte-for-byte: `*const char`
/// is rusty_v8's spelling of a C `const char*` (the bytes are read via a cast
/// to `c_char`).
///
/// # Safety
/// Invoked only by V8 as the isolate OOM callback; `location` and
/// `details.detail` are V8-owned nul-terminated strings (or null) valid for
/// the duration of the call.
pub(crate) unsafe extern "C" fn v8_oom_handler(
    location: *const std::os::raw::c_char,
    details: &v8::OomDetails,
) {
    // SAFETY: `location` is the V8-owned OOM-site pointer for this call;
    // `read_cstr` tolerates null and reads a nul-terminated C string.
    let site = unsafe { read_cstr(location) };
    // SAFETY: as above, for the V8-owned `details.detail` string (may be null).
    let extra = unsafe { read_cstr(details.detail) };
    let kind = if details.is_heap_oom {
        "JavaScript heap out of memory"
    } else {
        "process out of memory"
    };
    let detail = format!("FATAL: {kind}\n  at: {site}\n  detail: {extra}");
    report(&banner("crashed", "V8 out-of-memory", &detail));
    std::process::abort();
}

/// Read a nul-terminated C string (rusty_v8 `*const char`) into an owned
/// `String`, lossily. `<none>` on a null pointer.
///
/// # Safety
/// `p` must be null or point to a valid nul-terminated C string for the
/// duration of the call (upheld by V8, which owns the OOM location strings).
unsafe fn read_cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        return "<none>".to_string();
    }
    // SAFETY: `p` is non-null here (checked above) and points to a V8-owned
    // nul-terminated C string, so `CStr::from_ptr`'s precondition holds.
    unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_root_override_and_platform_dirs() {
        // Absolute in this host's own spelling: a relative platform variable
        // is ignored, and `/xdg` is not absolute on Windows.
        let base = std::env::temp_dir().join("oam-crash-probe");
        let at = |name: &str| base.join(name).to_string_lossy().into_owned();
        let all = |key: &str| {
            let value = match key {
                "OAM_CACHE_DIR" => at("override"),
                "LOCALAPPDATA" => at("lad"),
                "XDG_CACHE_HOME" => at("xdg"),
                _ => at("home"),
            };
            Some(value)
        };
        for windows in [true, false] {
            assert_eq!(
                resolve_cache_root(all, windows, PathBuf::from("/tmp-probe")),
                base.join("override")
            );
        }
        let no_override = |key: &str| match key {
            "OAM_CACHE_DIR" => None,
            other => all(other),
        };
        assert_eq!(
            resolve_cache_root(no_override, true, PathBuf::from("/tmp-probe")),
            base.join("lad").join("oam")
        );
        assert_eq!(
            resolve_cache_root(no_override, false, PathBuf::from("/tmp-probe")),
            base.join("xdg").join("oam")
        );
        let home_only = |key: &str| (key == "HOME").then(|| at("home"));
        assert_eq!(
            resolve_cache_root(home_only, false, PathBuf::from("/tmp-probe")),
            base.join("home").join(".cache").join("oam")
        );
    }

    /// Unset and set-but-empty variables both land under the temp dir, never
    /// in a cwd-relative path.
    #[test]
    fn cache_root_falls_back_to_temp_never_cwd() {
        let temp = std::env::temp_dir();
        for windows in [true, false] {
            let unset = resolve_cache_root(|_| None, windows, temp.clone());
            let empty = resolve_cache_root(|_| Some(String::new()), windows, temp.clone());
            assert_eq!(unset, temp.join("oam"));
            assert_eq!(empty, temp.join("oam"));
            assert!(unset.is_absolute(), "{}", unset.display());
        }
    }

    /// A relative `OAM_CACHE_DIR` is made absolute (so a later chdir cannot
    /// move it), and a relative platform variable is ignored, as the daemon's
    /// copy does.
    #[test]
    fn relative_values_never_name_a_cwd_relative_root() {
        let temp = std::env::temp_dir();
        let override_rel = |key: &str| (key == "OAM_CACHE_DIR").then(|| "rel".to_string());
        for windows in [true, false] {
            let root = resolve_cache_root(override_rel, windows, temp.clone());
            assert!(root.is_absolute(), "{}", root.display());
            assert!(root.ends_with("rel"), "{}", root.display());
            let all_rel = |key: &str| (key != "OAM_CACHE_DIR").then(|| "rel".to_string());
            assert_eq!(
                resolve_cache_root(all_rel, windows, temp.clone()),
                temp.join("oam")
            );
        }
    }

    /// Pruning keeps the newest `keep` crash files and touches nothing else.
    #[test]
    fn prune_keeps_the_newest_crash_files_only() {
        let dir = std::env::temp_dir().join(format!(
            "oam-crash-prune-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now();
        for i in 0..5u64 {
            let path = dir.join(format!("crash-{i}-1-0.txt"));
            std::fs::write(&path, b"x").unwrap();
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(now - std::time::Duration::from_secs(100 - i))
                .unwrap();
        }
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        prune_crash_files(&dir, 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["crash-3-1-0.txt", "crash-4-1-0.txt", "notes.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
