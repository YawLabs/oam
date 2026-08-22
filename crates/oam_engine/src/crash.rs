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

use std::backtrace::{Backtrace, BacktraceStatus};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

static HOOK_INSTALLED: Once = Once::new();

/// Short git SHA embedded at build time, if a build script exposes it. Absent
/// on a plain `cargo build` / tarball -> the banner shows "unknown". Wiring it
/// is a one-line `cargo:rustc-env=OAM_GIT_SHA=...` in a build.rs (left out here
/// to avoid git-dir rebuild churn; the version alone identifies a release).
const GIT_SHA: Option<&str> = option_env!("OAM_GIT_SHA");

/// Cache/state root, mirroring `oam_ts::daemon::cache_root` (duplicated so the
/// reporter stays self-contained and callable mid-crash with no cross-crate
/// state): `OAM_CACHE_DIR` override, else the per-platform user cache dir.
fn cache_root() -> PathBuf {
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

/// Milliseconds since the Unix epoch. `SystemTime` is permitted in runtime Rust
/// -- only JS `Date.now` is trapped for record/replay determinism.
fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The shared banner header. `kind` is a short crash class; `detail` is the
/// multi-line body (panic message + backtrace, or the OOM specifics).
fn banner(kind: &str, detail: &str) -> String {
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
        "\n======== oam crashed ({kind}) ========\n\
         oam v{ver} (git {sha}) on {os}-{arch}, pid {pid}\n\
         thread: {thread}\n\
         {detail}\n"
    )
}

/// Write `body` to `<cache>/crashes/crash-<millis>-<pid>.txt`, best-effort.
/// Returns the path when the write lands so the caller can point at it.
fn write_crash_file(body: &str) -> Option<PathBuf> {
    let dir = cache_root().join("crashes");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!(
        "crash-{}-{}.txt",
        unix_millis(),
        std::process::id()
    ));
    let mut file = std::fs::File::create(&path).ok()?;
    file.write_all(body.as_bytes()).ok()?;
    Some(path)
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
            report(&banner("panic", &detail));
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
// SAFETY: only V8 calls this, as the isolate OOM handler, with the exact
// `OomErrorCallback` C ABI; the string args are V8-owned (or null) and are read
// only through the null-tolerant `read_cstr` below.
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
    report(&banner("V8 out-of-memory", &detail));
    std::process::abort();
}

/// Read a nul-terminated C string (rusty_v8 `*const char`) into an owned
/// `String`, lossily. `<none>` on a null pointer.
///
/// # Safety
/// `p` must be null or point to a valid nul-terminated C string for the
/// duration of the call (upheld by V8, which owns the OOM location strings).
// SAFETY: `p` is null or a valid nul-terminated C string for the call, per the
// fn's `# Safety` contract above; the null case returns before any read.
unsafe fn read_cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        return "<none>".to_string();
    }
    // SAFETY: `p` is non-null here (checked above) and points to a V8-owned
    // nul-terminated C string, so `CStr::from_ptr`'s precondition holds.
    unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
}
