//! Windows: a child an oam program spawns dies when that oam process is KILLED.
//!
//! node gets this from libuv, which puts every non-detached child in a
//! process-global job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: the
//! parent's handle to the job is closed by the kernel however the parent ends,
//! and the job takes its members down with it. oam 0.15.1 had no job, so a
//! sidecar killed by its host left its browser running, and the repro --
//! a script that spawns `node -e "setInterval(()=>{},1e9)"` with stdio ignored,
//! then is killed -- found the grandchild still alive 1.5s later, where under
//! node it is gone.
//!
//! The parent here is a real `oam run` killed with `TerminateProcess`, so no
//! exit hook, drop, or `kill_on_drop` can stand in for the job. It reaches
//! every spawn path that backs `child_process` and `cluster` -- the tokio
//! `spawn`, the raw extra-fd `CreateProcessW`, `execFile`, `fork`,
//! `cluster.fork`, and `spawnSync` while it is still blocked -- and keeps a
//! `detached: true` child as the control, which must SURVIVE. Every child is
//! itself an `oam` (not node, which the box may not have) and reports its own
//! pid through a file, since a child `spawnSync` is blocked on has no
//! `child.pid` to read.
//!
//! No `unsafe`: liveness comes from `tasklist`, cleanup from `taskkill`.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Children that must die with the parent, one per spawn path.
const TIED: &[&str] = &[
    "spawn-ignore",
    "spawn-pipe",
    "spawn-extra-fd",
    "exec-file",
    "fork",
    "cluster",
    "spawn-sync",
];
/// The control: `detached: true` is how a child asks to outlive its parent.
const DETACHED: &str = "detached";

/// Each child: report the pid, then stay alive. The self-exit only bounds a
/// leak if this test process is itself killed before its cleanup runs.
const SLEEPER: &str = r#"
const fs = require("fs");
const path = require("path");
const kind = process.argv[2];
const dir = process.env.OAM_JOB_TEST_DIR;
const tmp = path.join(dir, kind + ".tmp");
fs.writeFileSync(tmp, String(process.pid));
fs.renameSync(tmp, path.join(dir, kind + ".pid"));
setInterval(() => {}, 1e9);
setTimeout(() => process.exit(0), 600000);
"#;

const PARENT: &str = r#"
const cluster = require("cluster");
const cp = require("child_process");
const fs = require("fs");
const path = require("path");
const dir = process.env.OAM_JOB_TEST_DIR;

if (cluster.isWorker) {
  const tmp = path.join(dir, "cluster.tmp");
  fs.writeFileSync(tmp, String(process.pid));
  fs.renameSync(tmp, path.join(dir, "cluster.pid"));
  setInterval(() => {}, 1e9);
  setTimeout(() => process.exit(0), 600000);
} else {
  const exe = process.execPath;
  const sleeper = path.join(dir, "sleeper.cjs");
  const args = (kind) => ["run", sleeper, "--no-check", "--", kind];
  const opts = { cwd: dir };

  cp.spawn(exe, args("spawn-ignore"), { ...opts, stdio: "ignore" });
  cp.spawn(exe, args("spawn-pipe"), opts);
  cp.spawn(exe, args("spawn-extra-fd"), { ...opts, stdio: ["ignore", "ignore", "ignore", "pipe"] });
  cp.execFile(exe, args("exec-file"), opts, () => {});
  cp.fork(sleeper, ["fork"], { ...opts, silent: true });
  cluster.fork();
  cp.spawn(exe, args("detached"), { ...opts, stdio: "ignore", detached: true }).unref();

  // spawnSync blocks the event loop, and fork/cluster.fork spawn from a
  // callback, so it goes last: once every other child is up.
  const before = ["spawn-ignore", "spawn-pipe", "spawn-extra-fd", "exec-file", "fork", "cluster", "detached"];
  const poll = setInterval(() => {
    if (!before.every((k) => fs.existsSync(path.join(dir, k + ".pid")))) return;
    clearInterval(poll);
    cp.spawnSync(exe, args("spawn-sync"), { ...opts, stdio: "ignore" });
  }, 25);
}
"#;

fn unique_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("oam-child-job-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// pid -> image name for every live `oam.exe`, from one `tasklist` call.
fn live_oam_pids() -> BTreeMap<u32, String> {
    let out = Command::new("tasklist")
        .args(["/FO", "CSV", "/NH", "/FI", "IMAGENAME eq oam.exe"])
        .output()
        .expect("tasklist runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut pids = BTreeMap::new();
    for line in text.lines() {
        // "oam.exe","1234","Console","1","12,345 K" -- the pid is the second
        // field, before the one quoted field that can hold a comma.
        let mut fields = line.split(',');
        let (Some(image), Some(pid)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let Ok(pid) = pid.trim_matches('"').parse::<u32>() {
            pids.insert(pid, image.trim_matches('"').to_string());
        }
    }
    pids
}

fn read_pid(dir: &Path, kind: &str) -> Option<u32> {
    std::fs::read_to_string(dir.join(format!("{kind}.pid")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Kills the parent and every reported child that is still an `oam.exe`, so a
/// RED run (or any failure) leaves nothing behind.
struct Cleanup {
    dir: PathBuf,
    parent: Child,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.parent.kill();
        let _ = self.parent.wait();
        let live = live_oam_pids();
        for kind in TIED.iter().chain(std::iter::once(&DETACHED)) {
            if let Some(pid) = read_pid(&self.dir, kind)
                && live.contains_key(&pid)
            {
                let _ = Command::new("taskkill")
                    .args(["/F", "/PID", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn parent_logs(dir: &Path) -> String {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
    format!(
        "--- parent stdout ---\n{}\n--- parent stderr ---\n{}",
        read("parent.out"),
        read("parent.err")
    )
}

#[test]
fn killing_oam_kills_its_children_but_not_detached_ones() {
    let dir = unique_dir();
    std::fs::write(dir.join("sleeper.cjs"), SLEEPER).unwrap();
    std::fs::write(dir.join("parent.cjs"), PARENT).unwrap();

    // Files, not pipes: a child that outlives the parent (the RED case) would
    // hold an inherited pipe end open and hang a read to EOF.
    let parent = Command::new(env!("CARGO_BIN_EXE_oam"))
        .args(["run", "parent.cjs", "--no-check"])
        .current_dir(&dir)
        .env("OAM_JOB_TEST_DIR", &dir)
        .env("OAM_CACHE_DIR", dir.join("cache"))
        .stdin(Stdio::null())
        .stdout(File::create(dir.join("parent.out")).unwrap())
        .stderr(File::create(dir.join("parent.err")).unwrap())
        .spawn()
        .expect("oam binary runs");
    let mut guard = Cleanup {
        dir: dir.clone(),
        parent,
    };

    // Eight cold oam starts on a possibly loaded box.
    let all: Vec<&str> = TIED.iter().copied().chain([DETACHED]).collect();
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        if all.iter().all(|k| read_pid(&dir, k).is_some()) {
            break;
        }
        if let Some(status) = guard.parent.try_wait().unwrap() {
            panic!(
                "parent exited ({status}) before every child reported\n{}",
                parent_logs(&dir)
            );
        }
        assert!(
            Instant::now() < deadline,
            "children did not all report within 240s; missing {:?}\n{}",
            all.iter()
                .filter(|k| read_pid(&dir, k).is_none())
                .collect::<Vec<_>>(),
            parent_logs(&dir)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let pids: BTreeMap<&str, u32> = all
        .iter()
        .map(|k| (*k, read_pid(&dir, k).unwrap()))
        .collect();

    // Non-vacuous: every child is alive while the parent is, so a dead one
    // afterwards was killed by the parent's death, not by exiting on its own.
    let live = live_oam_pids();
    let not_up: Vec<_> = pids.iter().filter(|(_, p)| !live.contains_key(p)).collect();
    assert!(
        not_up.is_empty(),
        "children already gone before the parent was killed: {not_up:?}\n{}",
        parent_logs(&dir)
    );

    // TerminateProcess: no JS, no Rust drop, no kill_on_drop runs in the parent.
    guard.parent.kill().expect("kill the oam parent");
    guard.parent.wait().expect("reap the oam parent");

    // The repro's check is at 1.5s; the extra grace absorbs a loaded box only.
    // Without the job the children never die, so it cannot turn RED green.
    std::thread::sleep(Duration::from_millis(1500));
    let grace = Instant::now() + Duration::from_secs(10);
    let survivors = loop {
        let live = live_oam_pids();
        let survivors: Vec<(&str, u32)> = TIED
            .iter()
            .map(|k| (*k, pids[k]))
            .filter(|(_, p)| live.contains_key(p))
            .collect();
        if survivors.is_empty() || Instant::now() >= grace {
            break survivors;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        survivors.is_empty(),
        "children outlived their killed oam parent: {survivors:?}"
    );

    assert!(
        live_oam_pids().contains_key(&pids[DETACHED]),
        "the detached child died with its parent; detached:true must stay out of the job"
    );
}
