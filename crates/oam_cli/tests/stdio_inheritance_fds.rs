//! The descriptors a parent hands oam above the three std ones (fd 3 and up)
//! must not leak into oam's children by inheritance.
//!
//! node runs uv_disable_stdio_inheritance at startup. On Windows that clears
//! HANDLE_FLAG_INHERIT on the std handles AND on every CRT fd the parent
//! passed in STARTUPINFO.lpReserved2; on unix it sets FD_CLOEXEC on the
//! inherited descriptors. A child then gets only the stdio it was configured
//! with. oam's startup covered the three std handles on Windows only
//! (stdio_inheritance.rs), so an extra-fd pipe (`stdio: [..., 'pipe']`) stayed
//! inheritable: every child oam started took a copy of the parent's pipe end
//! whatever its `stdio` said, and the parent's read of that pipe waited for
//! the last such child to exit instead of for oam.
//!
//! Shape: the test runs an oam OUTER script, which spawns an oam CHILD with
//! `stdio: ['ignore', 'ignore', 'inherit', 'ignore', 'pipe']` -- so fd 4 is a
//! real inherited descriptor in the child (a CRT fd from lpReserved2 on
//! Windows). fd 4, not 3: oam's extra-fd pipes are one-way, and by the CDP
//! convention its fd 3 is the one the child reads and fd 4 the one it writes.
//! The child writes `x` to fd 4, waits until the outer has received it,
//! starts a sleeper with every stdio slot ignored, and exits. The outer
//! reports what fd 4 carried and whether the sleeper's alive flag still
//! existed when fd 4 reached its end.
//!
//! Why the handshake: oam's own reader of a child's extra-fd pipe ends the
//! stream when the child is reaped if no read is in flight at that moment
//! (raw_wait closes the parent's idle pipe end), which node does not do. A
//! child that wrote `x` just before exiting raced that: the outer's read of
//! `x` could complete after the reap, the pipe end was then closed, and fd 4
//! "ended" at the child's exit whether or not the sleeper held it, so the
//! oracle passed with the leak in place. Writing `x` first and waiting for
//! the outer's got.flag puts the outer's next read in flight long before the
//! child exits, and an in-flight read ends only at the pipe's real EOF. On
//! Windows
//! the sleeper is detached, since a non-detached child dies with its parent's
//! kill-on-close job (child_job.rs); on unix it is an ordinary child, because
//! nothing there ends a child with its parent and every child inherits a
//! descriptor without FD_CLOEXEC, detached or not.
//!
//! The oracle is relative, not timed: with the leak, fd 4 cannot end before
//! the sleeper goes, and the sleeper lowers its flag before it goes. A
//! watchdog releases the sleeper a minute after its alive flag appears, so a
//! RED run fails instead of hanging, and a guard kills whatever is left. The
//! minute counts from the flag, not from the first spawn, so three slow oam
//! starts on a loaded host cannot release a live sleeper early and fail a
//! correct build; a sleeper that never starts fails with its own message.
//!
//! The regressions pin what the fix must keep: an inherited fd 4 is still
//! adopted and written (`fs.writeSync(4)`) and still passes through to a
//! grandchild named by number (`stdio: [..., 4]`); and `stdio: 'inherit'`
//! children still reach oam's stdout and stderr -- on unix that guards the
//! rule that FD_CLOEXEC is set on fds 3 and up only, never on 0-2, because
//! Rust std's `Stdio::inherit` leaves those in place rather than dup2-ing them
//! and a close-on-exec flag there would close the child's stdio at exec.
//!
//! No `unsafe`: liveness comes from flag files, cleanup from `taskkill` /
//! `kill`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the sleeper may take to raise its alive flag. SLEEPER_CHILD gives
/// up at the same bound.
const STARTUP_BOUND: Duration = Duration::from_secs(180);

/// How long after its alive flag appears the sleeper is released.
const RELEASE_AFTER: Duration = Duration::from_secs(60);

/// The sleeper: reports its pid, raises its alive flag, and runs until
/// released (stop.flag) or 90 s pass. It lowers the flag before it exits, so
/// the flag is down before its handles close.
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

/// Holds an inherited fd 4. Writes `x` to fd 4 and waits for the outer's
/// got.flag (see the module comment for why), then starts the sleeper
/// (detached when argv[2] says so) with every stdio slot ignored, waits for
/// its alive flag and exits.
const SLEEPER_CHILD: &str = r#"
const cp = require("child_process");
const fs = require("fs");
const path = require("path");
const dir = process.env.OAM_STDIO_TEST_DIR;
const alive = path.join(dir, "alive.flag");
const got = path.join(dir, "got.flag");
fs.writeSync(4, "x");
// Generous: process creation is slow on a loaded box. The test's watchdog
// uses the same 180 s bound.
const start = Date.now();
const waitGot = setInterval(() => {
  if (fs.existsSync(got)) {
    clearInterval(waitGot);
    startSleeper();
  } else if (Date.now() - start > 180000) {
    clearInterval(waitGot);
    console.error("the outer never received fd 4's x");
    process.exit(3);
  }
}, 20);
function startSleeper() {
  cp.spawn(process.execPath, ["run", "--no-check", path.join(dir, "sleeper.cjs")], {
    cwd: dir,
    detached: process.argv[2] === "detached",
    stdio: "ignore",
  }).unref();
  const poll = setInterval(() => {
    if (fs.existsSync(alive)) {
      clearInterval(poll);
      process.exit(0);
    } else if (Date.now() - start > 180000) {
      clearInterval(poll);
      console.error("the sleeper never raised its alive flag");
      process.exit(3);
    }
  }, 20);
}
"#;

/// Writes `c` to its inherited fd 4, then hands that fd by number to a
/// grandchild that writes `g` to its own fd 4, then exits.
const PASS_CHILD: &str = r#"
const cp = require("child_process");
const fs = require("fs");
fs.writeSync(4, "c");
const g = cp.spawn(process.execPath, ["-e", "require('fs').writeSync(4, 'g')"], {
  stdio: ["ignore", "ignore", "inherit", "ignore", 4],
});
g.on("error", (e) => {
  console.error("FAILED", e);
  process.exit(2);
});
g.on("exit", (status) => process.exit(status === 0 ? 0 : 4));
"#;

/// Spawns argv[2] (a script in the test dir) as an oam child with fd 4 a
/// pipe the child writes, passing argv[3] on. Raises got.flag once fd 4 has
/// delivered data. Reports what fd 4 carried, the child's exit status and
/// whether the sleeper's alive flag existed when fd 4 ended.
const OUTER: &str = r#"
const cp = require("child_process");
const fs = require("fs");
const path = require("path");
const dir = process.env.OAM_STDIO_TEST_DIR;
const alive = path.join(dir, "alive.flag");
const c = cp.spawn(
  process.execPath,
  ["run", "--no-check", path.join(dir, process.argv[2]), "--", process.argv[3] || ""],
  { cwd: dir, stdio: ["ignore", "ignore", "inherit", "ignore", "pipe"] },
);
let got = "";
let status = null;
let aliveAtEnd = null;
const report = () => {
  if (status !== null && aliveAtEnd !== null) {
    console.log(`fd4:${got} status:${status} alive-at-end:${aliveAtEnd}`);
    process.exit(0);
  }
};
c.stdio[4].on("data", (d) => {
  got += String(d);
  fs.writeFileSync(path.join(dir, "got.flag"), "");
});
c.stdio[4].on("end", () => {
  aliveAtEnd = fs.existsSync(alive);
  report();
});
c.on("error", (e) => {
  console.error("FAILED", e);
  process.exit(2);
});
c.on("exit", (s) => {
  status = s;
  report();
});
"#;

/// One child per spawn path, each with `stdio: 'inherit'` on fds 0-2, each
/// writing a marker to stdout and one to stderr; every spawn must succeed and
/// exit 0. The extra `'pipe'` slot sends the second through oam's raw
/// extra-fd spawn (child_unix / child_win) rather than Rust std's.
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

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "oam-stdio-fds-{tag}-{}-{nanos}",
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
/// cleanup kill against a recycled pid.
#[cfg(windows)]
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

/// True when `pid` is a live process whose command name is oam: guards the
/// cleanup kill against a recycled pid.
#[cfg(unix)]
fn is_live_oam(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().ends_with("oam"))
        .unwrap_or(false)
}

#[cfg(windows)]
fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn kill(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Releases the sleeper, gives it a moment to go on its own, then kills it if
/// it is still an oam, and removes the directory: a RED run, or any failure,
/// leaves nothing behind.
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
/// appears, unless told to stand down first: a RED run (fd 4 held until the
/// sleeper exits) then fails on the oracle instead of hanging for the
/// sleeper's full lifetime. The countdown starts at the flag, not at the
/// spawn, so startup time is never part of it. If the flag has not appeared
/// within `STARTUP_BOUND` the watchdog sets the returned "never started" flag
/// and writes stop.flag (so a late sleeper goes at once).
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

/// Runs OUTER over `child_script` (with `child_arg`) and returns its stdout
/// with line endings normalised, after asserting oam exited 0.
fn run_outer(dir: &Path, child_script: &str, child_arg: &str) -> String {
    let out = oam(dir)
        .args([
            "run",
            "--no-check",
            "outer.cjs",
            "--",
            child_script,
            child_arg,
        ])
        .stdin(Stdio::null())
        .output()
        .expect("oam binary runs");
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "oam run failed ({})\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    format!("{stdout}\nstderr: {stderr}")
}

/// The gating shape: an oam child holding an inherited fd 4 starts a sleeper
/// with every stdio slot ignored and exits; fd 4 must end while the sleeper
/// still lives.
fn fd4_ends_while_sleeper_lives(sleeper_mode: &str) {
    let dir = unique_dir(sleeper_mode);
    let _cleanup = Cleanup { dir: dir.clone() };
    std::fs::write(dir.join("sleeper.cjs"), SLEEPER).unwrap();
    std::fs::write(dir.join("child.cjs"), SLEEPER_CHILD).unwrap();
    std::fs::write(dir.join("outer.cjs"), OUTER).unwrap();

    let (stand_down, never_started) = watchdog(&dir);
    let report = run_outer(&dir, "child.cjs", sleeper_mode);
    let _ = stand_down.send(());

    assert!(
        !never_started.load(Ordering::SeqCst),
        "the sleeper never started (no alive.flag within {} s): the test could \
         not run, which says nothing about inheritance\n{report}",
        STARTUP_BOUND.as_secs()
    );

    assert!(
        report.starts_with("fd4:x status:0 "),
        "fd 4 content or the child's status:\n{report}"
    );
    assert!(
        report.starts_with("fd4:x status:0 alive-at-end:true\n"),
        "fd 4 ended only when the sleeper exited: the sleeper (stdio: 'ignore') \
         inherited the child's fd 4\n{report}"
    );
}

/// Windows: an extra-fd pipe the parent hands oam (CRT fd 4, from
/// lpReserved2) ends when oam exits, not when a detached child it started
/// with every stdio slot ignored exits.
#[cfg(windows)]
#[test]
fn detached_child_does_not_hold_an_inherited_extra_fd() {
    fd4_ends_while_sleeper_lives("detached");
}

/// unix: an inherited fd 4 ends when oam exits, not when an ordinary child it
/// started with every stdio slot ignored exits. Compiled and run on unix
/// only.
#[cfg(unix)]
#[test]
fn child_does_not_hold_an_inherited_fd_4() {
    fd4_ends_while_sleeper_lives("attached");
}

/// An inherited fd 4 is still adopted and written, and still passes through
/// to a grandchild that names it by number.
#[test]
fn inherited_fd_4_is_still_written_and_passed_through() {
    let dir = unique_dir("pass");
    let _cleanup = Cleanup { dir: dir.clone() };
    std::fs::write(dir.join("child.cjs"), PASS_CHILD).unwrap();
    std::fs::write(dir.join("outer.cjs"), OUTER).unwrap();

    let report = run_outer(&dir, "child.cjs", "");
    assert!(
        report.starts_with("fd4:cg status:0 alive-at-end:false\n"),
        "{report}"
    );
}

/// `stdio: 'inherit'` children still write to oam's stdout and stderr on Rust
/// std's spawn, oam's extra-fd spawn and spawnSync, and every one of those
/// spawns succeeds. On Windows this repeats stdio_inheritance.rs's guard; it
/// runs everywhere so the unix guard (no FD_CLOEXEC on 0-2) is exercised by
/// the same code that is checked here.
#[test]
fn inherit_children_still_reach_oam_stdout() {
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
