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
//! Why the handshake: up to 0.19.2 oam's reader of a child's extra-fd pipe
//! ended the stream when the child was reaped if no read was in flight at
//! that moment (raw_wait closed the parent's idle pipe end; fixed on Windows
//! by #419 and on unix by #456), which node does not do. A child that wrote
//! `x` just before exiting raced that: the outer's read of `x` could complete
//! after the reap, the pipe end was then closed, and fd 4 "ended" at the
//! child's exit whether or not the sleeper held it, so the oracle passed with
//! the leak in place. The handshake stays so the oracle does not depend on
//! that fix: writing `x` first and waiting for the outer's got.flag puts the
//! outer's next read in flight long before the child exits, and an in-flight
//! read ends only at the pipe's real EOF. On
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

// The sleeper script, the cleanup guard, the watchdog and the
// `stdio: 'inherit'` regression are shared with stdio_inheritance.rs.
mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;

use common::{Cleanup, SLEEPER, STARTUP_BOUND, oam, watchdog};

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

fn unique_dir(tag: &str) -> PathBuf {
    common::unique_dir("oam-stdio-fds", tag)
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
    common::assert_inherit_children_reach_oam_stdio(&dir);
}
