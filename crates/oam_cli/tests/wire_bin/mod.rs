//! Shared by the test targets that spawn oam and can be pointed at another
//! build of it (http_server_wire.rs, event_loop_heap.rs).
//!
//! Not a test target: cargo builds only the files directly under tests/ as
//! tests, and each of the two includes this with `mod wire_bin;`. Every item
//! here is reached from both (the sweep through `RunDir::new`), so neither
//! target sees dead code; an item only one of them uses needs
//! `#[allow(dead_code)]`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The oam binary these tests run: `OAM_WIRE_TEST_BIN` when it is set --
/// which runs a suite against another oam build, how each test was shown to
/// fail on the release before its fix -- otherwise the one cargo built for
/// this test target. A value that does not name a file fails the test
/// instead of quietly testing the cargo build in its place (the rule
/// `OAM_CONPTY_BIN` follows in conpty.rs); `var_os` keeps a path that is not
/// UTF-8 from reading as "unset".
pub fn oam_under_test() -> PathBuf {
    match std::env::var_os("OAM_WIRE_TEST_BIN").filter(|value| !value.is_empty()) {
        Some(bin) => {
            let bin = PathBuf::from(bin);
            assert!(
                bin.is_file(),
                "OAM_WIRE_TEST_BIN={} is not a file. Refusing to fall back to the \
                 cargo-built oam, which would pass for a binary nobody asked about.",
                bin.display()
            );
            bin
        }
        None => PathBuf::from(env!("CARGO_BIN_EXE_oam")),
    }
}

/// The name every run directory starts with.
pub const RUN_DIR_PREFIX: &str = "oam-wire-";

/// Names earlier versions of these targets gave their run directories, so
/// the sweep still removes what they left behind.
const LEGACY_RUN_DIR_PREFIXES: &[&str] = &["oam-http-wire-", "oam-loop-heap-"];

/// One run's own temp directory -- its scripts and its `OAM_CACHE_DIR` --
/// removed when the value is dropped, pass or fail (a failing test drops it
/// while unwinding). Of the two schemes these targets had -- this per-run
/// directory, or one directory per test process in a `OnceLock`, shared by
/// every test and never removed by the process that made it, with a sweep of
/// day-old ones -- the per-run directory is the one kept: it isolates
/// parallel tests from each other and cleans up after itself. The sweep stays
/// as a backstop for a process that is killed outright and never unwinds.
pub struct RunDir(PathBuf);

impl RunDir {
    /// A fresh, empty directory under the system temp directory. `label`
    /// only makes the name readable; uniqueness comes from the process id,
    /// the clock and a per-process counter, so tests running in parallel
    /// never share one.
    pub fn new(label: &str) -> Self {
        static SWEEP: Once = Once::new();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temp = std::env::temp_dir();
        SWEEP.call_once(|| sweep_stale_run_dirs(&temp));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = temp.join(format!(
            "{RUN_DIR_PREFIX}{}-{nanos}-{seq}-{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Write `content` to `name` (which may contain `/`) inside the run
    /// directory and return its path.
    pub fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    /// `oam <flags> run <script> --no-check` on [`oam_under_test`], with
    /// `OAM_CACHE_DIR` pointed into this run directory -- never the
    /// developer's own cache -- and `NODE_OPTIONS` cleared. `--no-check`
    /// starts no type-check daemon, so nothing the command starts outlives
    /// the child once it has exited or been killed.
    pub fn oam_run(&self, flags: &[&str], script: &Path) -> Command {
        let cache = self.0.join("oam-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let mut cmd = Command::new(oam_under_test());
        cmd.args(flags)
            .args(["run", script.to_str().unwrap(), "--no-check"])
            .env("OAM_CACHE_DIR", cache)
            .env_remove("NODE_OPTIONS");
        cmd
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How old a run directory must be before a later run removes it: a day, as
/// for e2e.rs's `oam-e2e-*` sweep -- far beyond any run, so a run in another
/// checkout never loses a directory it is still using.
pub const STALE_RUN_DIR_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Remove the run directories that processes killed before they could drop
/// them left in `temp`. Best effort: an entry that cannot be read or removed
/// is left for the next run.
fn sweep_stale_run_dirs(temp: &Path) {
    sweep_run_dirs_stale_at(temp, SystemTime::now());
}

/// [`sweep_stale_run_dirs`] with the clock as a parameter, so its test can
/// age a directory without rewriting its mtime.
pub fn sweep_run_dirs_stale_at(temp: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !std::iter::once(RUN_DIR_PREFIX)
            .chain(LEGACY_RUN_DIR_PREFIXES.iter().copied())
            .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        let path = entry.path();
        // symlink_metadata: a link named like a run dir is never followed.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age >= STALE_RUN_DIR_AGE);
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}
