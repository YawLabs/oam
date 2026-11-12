//! The harness stdio_inheritance.rs and stdio_inheritance_fds.rs share: the
//! sleeper script, the `stdio: 'inherit'` regression, the cleanup guard and
//! the watchdog. One copy, so a fix to the watchdog or the cleanup is made
//! once.
//!
//! Not a test target: cargo builds only the files directly under tests/ as
//! tests, and each of the two includes this with `mod common;`.
//!
//! No `unsafe`: liveness comes from flag files, cleanup from `taskkill` /
//! `kill`.

// Each test binary uses its own subset of this module.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the sleeper may take to raise its alive flag. The script that
/// starts it gives up at the same bound.
pub const STARTUP_BOUND: Duration = Duration::from_secs(180);

/// How long after its alive flag appears the sleeper is released.
pub const RELEASE_AFTER: Duration = Duration::from_secs(60);

/// The sleeper: reports its pid, raises its alive flag, and runs until
/// released (stop.flag) or 90 s pass. It lowers the flag before it exits, so
/// the flag is down before its handles close.
pub const SLEEPER: &str = r#"
const fs = require("fs");
const path = require("path");
const dir = process.env.OAM_STDIO_TEST_DIR;
const alive = path.join(dir, "alive.flag");
const stop = path.join(dir, "stop.flag");
fs.writeFileSync(path.join(dir, "sleeper.pid"), String(process.pid));
fs.writeFileSync(alive, "");
const end = Date.now() + 90000;
const t = setInterval(() => {
  if (Date.now() > end || fs.existsSync(stop)) {
    clearInterval(t);
    fs.rmSync(alive, { force: true });
  }
}, 200);
"#;

/// One child per spawn path, each with `stdio: 'inherit'` on fds 0-2, each
/// writing a marker to stdout and one to stderr; every spawn must succeed and
/// exit 0. The extra `'pipe'` slot sends the second through oam's raw
/// extra-fd spawn (child_unix / child_win) rather than Rust std's; on Windows
/// that path's inherit arm once passed oam's std handle itself and failed
/// with EINVAL once the handle was made non-inheritable.
pub const INHERIT_PARENT: &str = r#"
const cp = require("child_process");
const exe = process.execPath;
const code = (m) => `console.log(${JSON.stringify(m)}); console.error(${JSON.stringify(m + "-err")});`;
const run = (m, stdio) =>
  new Promise((resolve, reject) => {
    const c = cp.spawn(exe, ["-e", code(m)], { stdio });
    c.on("error", reject);
    c.on("exit", (status) =>
      status === 0 ? resolve() : reject(new Error(`${m} exited ${status}`)),
    );
  });
(async () => {
  await run("marker-tokio", "inherit");
  await run("marker-extra-slot", ["inherit", "inherit", "inherit", "pipe"]);
  const r = cp.spawnSync(exe, ["-e", code("marker-sync")], { stdio: "inherit" });
  if (r.error) throw r.error;
  if (r.status !== 0) throw new Error(`marker-sync exited ${r.status}`);
  console.log("all-spawned");
})().catch((e) => {
  console.error("FAILED", e);
  process.exit(2);
});
"#;

/// A fresh directory under the temp dir, named `<prefix>-<tag>-<pid>-<nanos>`.
pub fn unique_dir(prefix: &str, tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The oam under test, run in `dir` with the test's directory and a cache of
/// its own in the environment.
pub fn oam(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oam"));
    command
        .current_dir(dir)
        .env("OAM_STDIO_TEST_DIR", dir)
        .env("OAM_CACHE_DIR", dir.join("cache"));
    command
}

/// True when `pid` is a live `oam.exe`, from one `tasklist` call: guards the
/// cleanup kill against a recycled pid.
#[cfg(windows)]
pub fn is_live_oam(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args([
            "/FO",
            "CSV",
            "/NH",
            "/FI",
            &format!("PID eq {pid}"),
            "/FI",
            "IMAGENAME eq oam.exe",
        ])
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
        .unwrap_or(false)
}

/// True when `pid` is a live process whose command name is oam: guards the
/// cleanup kill against a recycled pid.
#[cfg(unix)]
pub fn is_live_oam(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().ends_with("oam"))
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
pub fn kill(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Releases the sleeper, gives it a moment to go on its own, then kills it if
/// it is still an oam, and removes the directory: a RED run, or any failure,
/// leaves nothing behind.
pub struct Cleanup {
    pub dir: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::write(self.dir.join("stop.flag"), "");
        for _ in 0..50 {
            if !self.dir.join("alive.flag").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let pid = std::fs::read_to_string(self.dir.join("sleeper.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if let Some(pid) = pid
            && is_live_oam(pid)
        {
            kill(pid);
        }
        // The sleeper's handle on its cwd can outlive the kill briefly.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Releases the sleeper (writes stop.flag) `RELEASE_AFTER` its alive flag
/// appears, unless told to stand down first: a RED run (the watched pipe held
/// until the sleeper exits) then fails on the oracle instead of hanging for
/// the sleeper's full lifetime. The countdown starts at the flag, not at the
/// spawn, so startup time is never part of it. If the flag has not appeared
/// within `STARTUP_BOUND` the watchdog sets the returned "never started" flag
/// and writes stop.flag (so a late sleeper goes at once).
pub fn watchdog(dir: &Path) -> (mpsc::Sender<()>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel::<()>();
    let alive = dir.join("alive.flag");
    let stop = dir.join("stop.flag");
    let never_started = Arc::new(AtomicBool::new(false));
    let never_started_w = Arc::clone(&never_started);
    std::thread::spawn(move || {
        let deadline = Instant::now() + STARTUP_BOUND;
        while !alive.exists() {
            if Instant::now() >= deadline {
                never_started_w.store(true, Ordering::SeqCst);
                let _ = std::fs::write(&stop, "");
                return;
            }
            if !matches!(
                rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                return;
            }
        }
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(RELEASE_AFTER) {
            let _ = std::fs::write(stop, "");
        }
    });
    (tx, never_started)
}

/// Runs INHERIT_PARENT in `dir` and asserts that every `stdio: 'inherit'`
/// child's markers reached oam's stdout and stderr and every spawn succeeded.
pub fn assert_inherit_children_reach_oam_stdio(dir: &Path) {
    std::fs::write(dir.join("parent.cjs"), INHERIT_PARENT).unwrap();

    let out = oam(dir)
        .args(["run", "--no-check", "parent.cjs"])
        .stdin(Stdio::piped())
        .output()
        .expect("oam binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    assert!(
        out.status.success(),
        "oam run failed ({})\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    for marker in ["marker-tokio", "marker-extra-slot", "marker-sync"] {
        assert!(
            stdout.lines().any(|l| l == marker),
            "{marker} missing from oam's stdout\nstdout: {stdout}\nstderr: {stderr}"
        );
        let err_marker = format!("{marker}-err");
        assert!(
            stderr.lines().any(|l| l == err_marker),
            "{err_marker} missing from oam's stderr\nstdout: {stdout}\nstderr: {stderr}"
        );
    }
    assert!(
        stdout.lines().any(|l| l == "all-spawned"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}
