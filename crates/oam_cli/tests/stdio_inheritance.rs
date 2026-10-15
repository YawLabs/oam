//! Windows: oam's own std handles must not leak into its children by
//! inheritance.
//!
//! node makes its stdin, stdout and stderr handles non-inheritable at startup
//! (PlatformInit -> uv_disable_stdio_inheritance). A child then gets only the
//! stdio it was configured with: `stdio: 'inherit'` receives a fresh
//! inheritable duplicate, and `stdio: 'ignore'` receives nothing of the
//! parent's. oam left the inherit bit set, and its spawns pass
//! `bInheritHandles = TRUE`, so every child took a copy of oam's std handles
//! whatever its `stdio` said. A non-detached child dies with oam (the
//! kill-on-close job, see child_job.rs), but a `detached: true` child does
//! not: it kept oam's stdout and stderr pipes open after oam exited, and
//! `$(oam run x.js)`, `oam run x.js | tail` or a CI log capture waited for it
//! to exit -- an "exit delay" the script never asked for.
//!
//! The gating oracle is relative, not timed: the reader sees EOF on oam's
//! stdout (or stderr) while the detached grandchild's alive flag still
//! exists. With the leak, EOF cannot arrive before the grandchild goes, and
//! the grandchild removes its flag before it goes. A watchdog releases the
//! grandchild a minute after its alive flag appears, so a RED run fails
//! instead of hanging, and a guard kills whatever is left. The minute counts
//! from the flag, not from the spawn, so a slow start on a loaded host cannot
//! release a live grandchild early and fail a correct build; a grandchild
//! that never starts fails with its own message.
//!
//! The regressions pin what the fix must keep: children with
//! `stdio: 'inherit'` still write to oam's stdout and stderr on every spawn
//! path (tokio spawn, the raw extra-fd CreateProcessW, spawnSync), and a
//! detached child with `stdio: 'pipe'` still delivers its output.
//!
//! No `unsafe`: liveness comes from flag files, cleanup from `taskkill`.

#![cfg(windows)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the grandchild may take to raise its alive flag. PARENT gives up
/// at the same bound.
const STARTUP_BOUND: Duration = Duration::from_secs(180);

/// How long after its alive flag appears the grandchild is released.
const RELEASE_AFTER: Duration = Duration::from_secs(60);

/// The detached grandchild: reports its pid, raises its alive flag, and runs
/// until released (stop.flag) or 90 s pass. It lowers the flag before it
/// exits, so the flag is down before its handles close.
const SLEEPER: &str = r#"
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

/// Starts the sleeper detached with every stdio slot ignored, waits for its
/// alive flag, writes `done` to the stream named by argv[2], and exits.
const PARENT: &str = r#"
const cp = require("child_process");
const fs = require("fs");
const path = require("path");
const dir = process.env.OAM_STDIO_TEST_DIR;
const stream = process.argv[2];
const alive = path.join(dir, "alive.flag");
const sleeper = path.join(dir, "sleeper.cjs");
cp.spawn(process.execPath, ["run", "--no-check", sleeper], {
  cwd: dir,
  detached: true,
  stdio: "ignore",
}).unref();
// Generous: process creation is slow on a loaded box. The test's watchdog
// uses the same 180 s bound.
const start = Date.now();
const poll = setInterval(() => {
  if (fs.existsSync(alive)) {
    clearInterval(poll);
    if (stream === "stderr") console.error("done");
    else console.log("done");
    process.exit(0);
  } else if (Date.now() - start > 180000) {
    clearInterval(poll);
    console.error("the sleeper never raised its alive flag");
    process.exit(3);
  }
}, 20);
"#;

/// One child per spawn path, each with `stdio: 'inherit'` on fds 0-2, each
/// writing a marker to stdout and one to stderr; every spawn must succeed and
/// exit 0. The extra `'pipe'` slot sends the second through child_win's raw
/// CreateProcessW, whose inherit arm once passed oam's std handle itself and
/// failed with EINVAL once the handle was made non-inheritable.
const INHERIT_PARENT: &str = r#"
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

/// A detached child with piped stdio: its output still reaches the parent.
const DETACHED_PIPE_PARENT: &str = r#"
const cp = require("child_process");
const c = cp.spawn(process.execPath, ["-e", "console.log('X')"], {
  detached: true,
  stdio: "pipe",
});
let out = "";
c.stdout.setEncoding("utf8");
c.stdout.on("data", (d) => (out += d));
c.on("error", (e) => {
  console.error("FAILED", e);
  process.exit(2);
});
c.on("close", (status) => console.log(`got:${out.trim()} status:${status}`));
"#;

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "oam-stdio-inherit-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn oam(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oam"));
    command
        .current_dir(dir)
        .env("OAM_STDIO_TEST_DIR", dir)
        .env("OAM_CACHE_DIR", dir.join("cache"));
    command
}

/// True when `pid` is a live `oam.exe`, from one `tasklist` call: guards the
/// cleanup `taskkill` against a recycled pid.
fn is_live_oam(pid: u32) -> bool {
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

/// Releases the sleeper, gives it a moment to go on its own, then kills it if
/// it is still an `oam.exe`, and removes the directory: a RED run, or any
/// failure, leaves nothing behind.
struct Cleanup {
    dir: PathBuf,
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
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // The sleeper's handle on its cwd can outlive taskkill briefly.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.dir).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Releases the grandchild (writes stop.flag) `RELEASE_AFTER` its alive flag
/// appears, unless told to stand down first: a RED run (EOF held until the
/// grandchild exits) then fails on the oracle instead of hanging for the
/// grandchild's full lifetime. The countdown starts at the flag, not at the
/// spawn, so startup time is never part of it. If the flag has not appeared
/// within `STARTUP_BOUND` the watchdog sets the returned "never started" flag
/// and writes stop.flag (so a late grandchild goes at once).
fn watchdog(dir: &Path) -> (mpsc::Sender<()>, Arc<AtomicBool>) {
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

#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Stdout,
    Stderr,
}

/// Runs PARENT with all three std handles piped and reads `stream` to EOF.
/// The other stream is drained on a thread so neither pipe can fill. Asserts
/// that `stream` carried exactly `done` and that the grandchild's alive flag
/// still existed when its EOF arrived.
fn eof_while_detached_grandchild_lives(stream: Stream) {
    let dir = unique_dir(if stream == Stream::Stdout {
        "out"
    } else {
        "err"
    });
    let _cleanup = Cleanup { dir: dir.clone() };
    std::fs::write(dir.join("sleeper.cjs"), SLEEPER).unwrap();
    std::fs::write(dir.join("parent.cjs"), PARENT).unwrap();

    let mut child = oam(&dir)
        .args(["run", "--no-check", "parent.cjs", "--"])
        .arg(if stream == Stream::Stdout {
            "stdout"
        } else {
            "stderr"
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oam binary runs");
    let (stand_down, never_started) = watchdog(&dir);

    let (mut watched, other): (Box<dyn Read + Send>, Box<dyn Read + Send>) = match stream {
        Stream::Stdout => (
            Box::new(child.stdout.take().unwrap()),
            Box::new(child.stderr.take().unwrap()),
        ),
        Stream::Stderr => (
            Box::new(child.stderr.take().unwrap()),
            Box::new(child.stdout.take().unwrap()),
        ),
    };
    let drain = std::thread::spawn(move || {
        let mut other = other;
        let mut buf = Vec::new();
        let _ = other.read_to_end(&mut buf);
        buf
    });

    let mut got = Vec::new();
    watched.read_to_end(&mut got).unwrap();
    // The oracle: was the grandchild still running when EOF arrived?
    let held_at_eof = dir.join("alive.flag").exists();
    let _ = stand_down.send(());
    drop(child.stdin.take());
    let status = child.wait().unwrap();
    // Unblocked by the release in Cleanup at worst; read now that the flag
    // has been sampled.
    let _ = std::fs::write(dir.join("stop.flag"), "");
    let other = drain.join().unwrap();

    let got = String::from_utf8_lossy(&got);
    let other = String::from_utf8_lossy(&other);
    assert!(
        !never_started.load(Ordering::SeqCst),
        "the detached grandchild never started (no alive.flag within {} s): the \
         test could not run, which says nothing about inheritance\nwatched: \
         {got}\nother: {other}",
        STARTUP_BOUND.as_secs()
    );
    assert!(
        status.success(),
        "oam run failed ({status})\nwatched: {got}\nother: {other}"
    );
    assert_eq!(
        got.replace("\r\n", "\n"),
        "done\n",
        "watched stream content; other stream: {other}"
    );
    assert!(
        held_at_eof,
        "EOF on oam's {} waited for the detached grandchild to exit: the \
         grandchild (stdio: 'ignore') inherited oam's std handle",
        if stream == Stream::Stdout {
            "stdout"
        } else {
            "stderr"
        }
    );
}

/// oam's stdout reaches EOF when oam exits, not when a detached child it
/// started (with every stdio slot ignored) exits.
#[test]
#[ignore = "gap 2: fails until oam makes its std handles non-inheritable at startup; the fix removes this attribute and nothing else"]
fn detached_child_does_not_hold_oam_stdout() {
    eof_while_detached_grandchild_lives(Stream::Stdout);
}

/// The same for oam's stderr (std handle -12 as well as -11).
#[test]
#[ignore = "gap 2: fails until oam makes its std handles non-inheritable at startup; the fix removes this attribute and nothing else"]
fn detached_child_does_not_hold_oam_stderr() {
    eof_while_detached_grandchild_lives(Stream::Stderr);
}

/// `stdio: 'inherit'` children still write to oam's stdout and stderr on the
/// tokio spawn path, child_win's extra-slot path and spawnSync, and every one
/// of those spawns succeeds.
#[test]
fn inherit_children_still_write_to_oam_stdout() {
    let dir = unique_dir("inherit");
    let _cleanup = Cleanup { dir: dir.clone() };
    std::fs::write(dir.join("parent.cjs"), INHERIT_PARENT).unwrap();

    let out = oam(&dir)
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

/// A detached child with `stdio: 'pipe'` still delivers its output to oam.
#[test]
fn detached_pipe_child_still_delivers() {
    let dir = unique_dir("detpipe");
    let _cleanup = Cleanup { dir: dir.clone() };
    std::fs::write(dir.join("parent.cjs"), DETACHED_PIPE_PARENT).unwrap();

    let out = oam(&dir)
        .args(["run", "--no-check", "parent.cjs"])
        .stdin(Stdio::piped())
        .output()
        .expect("oam binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "oam run failed ({})\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert_eq!(stdout, "got:X status:0\n", "stderr: {stderr}");
}
