//! `cargo xtask bench`: micro-benchmark harness for the oam JS runtime.
//!
//! Eleven benchmark cases measure different layers of the stack:
//!   1. cold-start             -- process start to exit on a trivial script
//!   2. url-parse              -- URL constructor throughput (10k parses)
//!   3. http-throughput        -- node:http + fetch loopback (200 req)
//!   4. http-keepalive-latency -- per-request ms of small fetches on one pooled connection
//!   5. fs-read                -- fs.readFileSync hot-loop (1000 reads of 4KB)
//!   6. json-parse             -- JSON.parse + JSON.stringify (1000 round-trips)
//!   7. crypto-hash            -- SHA-256 hash of 64KB (1000 iterations)
//!   8. mcp-cold-start         -- process spawn to first MCP initialize response
//!   9. mcp-idle-rss           -- RSS (MB) after initialize, server idle
//!  10. mcp-first-call-latency -- tools/call round-trip on warm server
//!  11. ts-cold-start          -- 20-module TypeScript graph load, per cache state
//!
//! With `--compare`, the same scripts run under every runtime found on
//! PATH (node, bun, deno) and the harness reports a comparison table.
//!
//! With `--case <name>` (repeatable) only those cases run, and their rows are
//! MERGED into the committed files rather than replacing them: the header
//! stamp keeps describing the last full run, each re-measured row carries its
//! own commit, and `ts-cold-start` -- which has its own section because it
//! publishes several rows per runtime -- carries its own stamp.
//!
//! Outputs: bench/results.json (machine) and BENCHMARKS.md (human),
//! both COMMITTED -- the receipt is in the repo.

use crate::conformance::{
    capture_version, ensure_oam_built, git_short_commit, repo_root, run_with_timeout,
};

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ config

// Sample counts: how many times each case is run, with the MEDIAN published.
//
// These were 10 / 5 / 3 and that was too few to publish. Three runs of
// UNCHANGED code disagreed by up to 40%, and each one was an outlier somewhere
// different -- `f096b5d` on cold-start (node 168.84ms against ~113ms in the runs
// either side), `acec008` on crypto-hash (node 292.29ms against ~205ms). Node's
// binary was byte-identical across all three, so every bit of that spread was
// sampling noise being published as measurement, and it made the tables look
// like oam had regressed when nothing about oam had changed.
//
// A median over n=5 is not a stable statistic. n=15 costs about 3x the wall
// clock on a run that already takes minutes, which is cheap for a number that
// goes in the README and gets quoted at people. Raise these before trusting a
// tighter comparison, not after.
//
// Load still matters more than n: do not measure on a busy machine. The
// interleaving in `run_timed_case` protects the RATIOS from uniform drift, but
// nothing protects the absolutes from a test suite running alongside.
const COLD_START_ITERS: usize = 20;
const TIMED_ITERS: usize = 15;
const HTTP_ITERS: usize = 10;
/// Per-sample, not per-case -- each sample is its own process spawn.
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

/// Every case, in the order the tables print them. `--case` names come from
/// this list and nowhere else.
const CASE_NAMES: [&str; 11] = [
    "cold-start",
    "url-parse",
    "http-throughput",
    "http-keepalive-latency",
    "fs-read",
    "json-parse",
    "crypto-hash",
    "mcp-cold-start",
    "mcp-idle-rss",
    "mcp-first-call-latency",
    TS_CASE,
];
/// The one case that does not fit the main table (several rows per runtime),
/// so it gets its own section in both output files.
const TS_CASE: &str = "ts-cold-start";

/// The cases that share the main `case x runtime` table.
fn main_cases() -> impl Iterator<Item = &'static str> {
    CASE_NAMES.iter().copied().filter(|c| *c != TS_CASE)
}

/// Resolve `--case` filters against `CASE_NAMES`, preserving table order. An
/// unknown name is an error rather than a silent no-op run: a typo that
/// measured nothing and then "merged" nothing would look like success.
fn select_cases(only: &[String]) -> Result<Vec<&'static str>> {
    if only.is_empty() {
        return Ok(CASE_NAMES.to_vec());
    }
    for name in only {
        if !CASE_NAMES.contains(&name.as_str()) {
            bail!(
                "unknown case `{name}` -- the cases are: {}",
                CASE_NAMES.join(", ")
            );
        }
    }
    Ok(CASE_NAMES
        .iter()
        .copied()
        .filter(|c| only.iter().any(|o| o == c))
        .collect())
}

// ----------------------------------------------------------------- runtime

struct Runtime {
    name: String,
    exe: PathBuf,
    version: String,
}

impl Runtime {
    fn build_cmd(&self, script: &Path, extra_args: &[String]) -> Command {
        let mut cmd = Command::new(&self.exe);
        match self.name.as_str() {
            "oam" => {
                cmd.arg("run").arg(script).arg("--no-check");
                if !extra_args.is_empty() {
                    cmd.arg("--");
                    cmd.args(extra_args);
                }
            }
            "deno" => {
                cmd.arg("run").arg("--allow-all").arg(script);
                cmd.args(extra_args);
            }
            "bun" => {
                cmd.arg("run").arg(script);
                cmd.args(extra_args);
            }
            _ => {
                cmd.arg(script);
                cmd.args(extra_args);
            }
        }
        cmd
    }
}

/// Removes the staging directory when the benchmark run ends.
struct StagedBinary {
    dir: PathBuf,
}

impl Drop for StagedBinary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Copy the just-built oam binary out of `target/`, then exec it once before
/// any timing starts.
///
/// WHY THIS EXISTS
/// `target/` is not a stable place to measure from. Its contents change
/// underneath a running benchmark: a concurrent `cargo build` -- another
/// terminal, an editor checking on save, a sibling agent -- replaces the binary
/// mid-run, and freshly-written bytes are cold where the installed `node.exe`
/// and `bun.exe` they are compared against are warm. The asymmetry lands
/// entirely on oam, the one binary under test.
///
/// CORRECTION, recorded because the first version of this comment was wrong.
/// It claimed an on-access scanner rescans build outputs on EVERY exec and
/// cited 306 ms from `target/` against 61 ms staged -- a 5.0x handicap. That
/// does NOT reproduce: the same interleaved comparison on a settled tree
/// measures 1.03x. The original numbers were taken while a concurrent session
/// was rebuilding oam, so each exec hit different bytes -- mtime and size both
/// moved during the run. It was a moving file, not a permanent per-exec scan.
///
/// The mitigation is unchanged and still correct: a private copy is stable for
/// the duration of the run no matter what cargo does to `target/`. But it
/// defends against a moving target rather than a scanner, and the magnitude is
/// situational rather than a fixed 5x.
///
/// Staging disables nothing and needs no privileges. Where nothing is
/// rebuilding concurrently it is a file copy that changes no result.
///
/// KNOWN ASYMMETRY, and it favours oam. The pre-exec below absorbs a
/// first-execution cost -- notably an on-access malware scan of the image --
/// once, outside the timed loop. node and bun get no equivalent: they are
/// exec'd from their install directories on every iteration, so if a scanner is
/// scanning per exec, they pay it every time and oam does not. Measured on this
/// machine 2026-08-10 with McAfee on-access active: `node --version`, which
/// exits before node's own bootstrap, took 770-850ms against a historical
/// ~113ms cold-start, while `cmd /c exit` (small, in System32, trusted) stayed
/// at 42ms. A run taken in that state reported oam at 0.04x node on
/// `mcp-cold-start` -- roughly an order of magnitude better than every
/// neighbouring run -- and was discarded rather than published.
///
/// So: `cold-start`, `mcp-cold-start` and `mcp-first-call-latency` are only
/// trustworthy on a machine with no per-exec scanning, and the in-process cases
/// (`url-parse`, `json-parse`, `crypto-hash`) are the ones that stay honest when
/// it is present -- they were within a few percent of the previous run in the
/// same discarded measurement. Sanity-check a spawn-bound result against the
/// others before believing it; if the compute cases match history and the spawn
/// cases moved by multiples, the machine moved, not the runtime.
///
/// Failure is non-fatal -- a bench run with a warning beats no bench run.
fn stage_for_benchmark(oam: &Path) -> Result<(PathBuf, StagedBinary)> {
    let file_name = oam
        .file_name()
        .context("oam binary path has no file name")?
        .to_owned();
    // PID-suffixed so two runs on one machine cannot collide.
    let dir = std::env::temp_dir().join(format!("oam-bench-stage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating staging dir {}", dir.display()))?;
    let staged = dir.join(file_name);
    // fs::copy carries permission bits, so the exec bit survives on unix.
    std::fs::copy(oam, &staged)
        .with_context(|| format!("copying {} to {}", oam.display(), staged.display()))?;
    // Absorb the one-time scan here, outside every measured iteration.
    let _ = Command::new(&staged)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok((staged, StagedBinary { dir }))
}

fn discover_runtimes(oam: PathBuf, oam_version: &str) -> Vec<Runtime> {
    let mut runtimes = vec![Runtime {
        name: "oam".to_string(),
        exe: oam,
        version: oam_version.to_string(),
    }];
    for (name, exe_name, version_args) in [
        ("node", "node", vec!["--version"]),
        ("bun", "bun", vec!["--version"]),
        ("deno", "deno", vec!["--version"]),
    ] {
        if let Ok(path) = which(exe_name) {
            let version = capture_version(&path, &version_args);
            runtimes.push(Runtime {
                name: name.to_string(),
                exe: path,
                version,
            });
        }
    }
    runtimes
}

/// Portable PATH lookup. The previous impl shelled out to `where`, which is
/// Windows-only -- on Unix it does not exist, so node/bun/deno were silently
/// dropped and `--compare` benchmarked oam alone. Walk PATH ourselves instead,
/// trying the Windows executable suffixes where applicable.
fn which(name: &str) -> Result<PathBuf> {
    let path_var = std::env::var_os("PATH").context("PATH is not set")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    which_in(name, &dirs, cfg!(windows)).with_context(|| format!("{name} not found on PATH"))
}

/// The lookup behind [`which`], with the directory list and the platform
/// passed in so the suffix order is testable.
///
/// On Windows the executable suffixes are tried BEFORE the bare name. An
/// npm-installed tool leaves both `bun` (an sh shim, for Git Bash) and
/// `bun.cmd` in one directory; the bare name is a shell script `Command`
/// cannot exec, so taking it first made that runtime fail every case.
fn which_in(name: &str, dirs: &[PathBuf], windows: bool) -> Option<PathBuf> {
    let exts: &[&str] = if windows {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in dirs {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// ------------------------------------------------------------ case runner

struct CaseResult {
    runtime: String,
    name: String,
    median: f64,
    min: f64,
    max: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    unit: String,
    iterations: usize,
    warmup_iterations: usize,
    samples: Vec<f64>,
}

/// Returns (p50, min, max, p95, p99) from sorted samples.
fn stats(samples: &[f64]) -> (f64, f64, f64, f64, f64) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let p50 = if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    };
    let p95 = sorted[((95.0 / 100.0 * n as f64).round() as usize).clamp(1, n) - 1];
    let p99 = sorted[((99.0 / 100.0 * n as f64).round() as usize).clamp(1, n) - 1];
    (p50, sorted[0], sorted[n - 1], p95, p99)
}

/// Sample every runtime for one case, INTERLEAVED: one iteration of each
/// runtime per round, round-robin, rather than one runtime's whole block
/// before the next runtime starts.
///
/// Why this matters more than it looks. Blocked sampling makes the comparison
/// hostage to whatever else the machine is doing: a load spike that lands
/// during runtime A's block inflates A's median and never touches B's, so the
/// ratio the table reports is drift, not a difference between the runtimes.
/// The failure is silent -- every individual timing is real, the run reports no
/// error, and the conclusion is simply wrong.
///
/// That is not hypothetical here. Measuring under contention on this project
/// produced three separate wrong conclusions in one session ("oam is a
/// cold-start regression", later a claimed 5.0x scanner penalty), each of which
/// took a deliberate re-measurement to overturn. Interleaving spreads any spike
/// across every runtime in the same round, so drift largely cancels in the
/// comparison instead of being attributed to one side.
///
/// A failing runtime is recorded and skipped for the remaining rounds rather
/// than aborting the case, so one broken runtime cannot destroy the others'
/// samples. `stage_for_benchmark` handles the orthogonal problem of the binary
/// being replaced mid-run.
fn run_timed_case_interleaved(
    case_name: &str,
    runtimes: &[Runtime],
    script: &Path,
    iters: usize,
    extra_args: &[String],
) -> Vec<(String, Result<CaseResult>)> {
    let mut samples: Vec<Vec<f64>> = vec![Vec::with_capacity(iters); runtimes.len()];
    let mut failed: Vec<Option<String>> = vec![None; runtimes.len()];

    // One line per case rather than per runtime: the rounds interleave, so
    // per-runtime progress lines would interleave too and read as noise.
    let names: Vec<&str> = runtimes.iter().map(|r| r.name.as_str()).collect();
    print!("  {case_name} [{}]: ", names.join(" "));
    let _ = std::io::Write::flush(&mut std::io::stdout());

    for i in 0..iters {
        for (idx, rt) in runtimes.iter().enumerate() {
            if failed[idx].is_some() {
                continue;
            }
            match sample_once(case_name, rt, script, extra_args, i) {
                Ok(v) => samples[idx].push(v),
                Err(e) => failed[idx] = Some(e.to_string()),
            }
        }
        print!(".");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    println!(" done");

    runtimes
        .iter()
        .enumerate()
        .map(|(idx, rt)| {
            let name = rt.name.clone();
            if let Some(err) = &failed[idx] {
                return (name, Err(anyhow::anyhow!("{err}")));
            }
            let s = std::mem::take(&mut samples[idx]);
            let (p50, min, max, p95, p99) = stats(&s);
            (
                rt.name.clone(),
                Ok(CaseResult {
                    runtime: rt.name.clone(),
                    name: case_name.to_string(),
                    median: p50,
                    min,
                    max,
                    p50,
                    p95,
                    p99,
                    unit: "ms".to_string(),
                    iterations: s.len(),
                    warmup_iterations: 0,
                    samples: s,
                }),
            )
        })
        .collect()
}

/// One timed iteration. Split out of the sampling loop so the interleaved
/// driver can take a single sample per runtime per round.
fn sample_once(
    case_name: &str,
    rt: &Runtime,
    script: &Path,
    extra_args: &[String],
    i: usize,
) -> Result<f64> {
    let out = run_with_timeout(&mut rt.build_cmd(script, extra_args), CASE_TIMEOUT)?;
    if out.timed_out {
        bail!("{}/{case_name}: iteration {i} timed out", rt.name);
    }
    if out.code != 0 {
        bail!(
            "{}/{case_name}: iteration {i} exit {}; stderr: {}",
            rt.name,
            out.code,
            out.stderr.chars().take(200).collect::<String>()
        );
    }
    let v: serde_json::Value = serde_json::from_str(out.stdout.trim()).with_context(|| {
        format!(
            "{}/{case_name}: iter {i} parse failed: {}",
            rt.name,
            out.stdout.chars().take(100).collect::<String>()
        )
    })?;
    v["elapsed_ms"].as_f64().context("missing elapsed_ms")
}

fn run_cold_start(rt: &Runtime, script: &Path) -> Result<CaseResult> {
    print!("  {}/cold-start: ", rt.name);
    let mut samples = Vec::with_capacity(COLD_START_ITERS);
    for i in 0..COLD_START_ITERS {
        let start = Instant::now();
        let out = run_with_timeout(&mut rt.build_cmd(script, &[]), CASE_TIMEOUT)?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        if out.timed_out {
            bail!("{}/cold-start: iteration {i} timed out", rt.name);
        }
        if out.code != 0 {
            bail!("{}/cold-start: iteration {i} exit {}", rt.name, out.code);
        }
        samples.push(elapsed_ms);
        print!(".");
    }
    println!(" done");

    let (p50, min, max, p95, p99) = stats(&samples);
    Ok(CaseResult {
        runtime: rt.name.clone(),
        name: "cold-start".to_string(),
        median: p50,
        min,
        max,
        p50,
        p95,
        p99,
        unit: "ms".to_string(),
        iterations: COLD_START_ITERS,
        warmup_iterations: 0,
        samples,
    })
}

/// One MCP server process under measurement, driven over stdio.
///
/// Two things the mcp-* cases need on EVERY path, which is why they are a
/// type rather than inline code:
///
/// * a deadline on each response. Stdout is read on its own thread and the
///   lines arrive over a channel, so a server that starts but never answers
///   fails the sample after `CASE_TIMEOUT` instead of hanging the bench on a
///   blocking read;
/// * the child is killed and reaped when this drops, so an error return (a
///   timeout, a closed pipe, a failed write) cannot leave a server running.
///
/// Stdin stays open until drop: closing it is EOF, which a server may treat
/// as "no more input" and exit on before writing its response.
struct McpServer {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    /// Each non-empty stdout line, with the instant its read returned. The
    /// clock stops on the reader thread, so a figure carries no harness
    /// scheduling between the line landing and the main thread noticing.
    lines: std::sync::mpsc::Receiver<(Instant, String)>,
}

impl McpServer {
    fn spawn(mut cmd: Command) -> Result<Self> {
        use std::io::BufRead;

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("spawn MCP server")?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (tx, lines) = std::sync::mpsc::channel();
        // Built before the pipes are checked, so the drop below reaps the
        // child if either is missing.
        let server = McpServer {
            child,
            stdin,
            lines,
        };
        if server.stdin.is_none() {
            bail!("mcp server stdin was not piped");
        }
        let stdout = stdout.context("mcp server stdout")?;
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    // EOF or a read error: dropping `tx` tells the receiver.
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let at = Instant::now();
                        if !line.trim().is_empty() && tx.send((at, line.clone())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(server)
    }

    /// Write one newline-terminated JSON-RPC message.
    fn send(&mut self, msg: &str) -> Result<()> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().context("mcp server stdin")?;
        stdin.write_all(msg.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }

    /// The instant the next non-empty stdout line was read, or an error
    /// naming `what` when the server closes stdout first or stays silent past
    /// `timeout`.
    fn next_line(&self, what: &str, timeout: Duration) -> Result<Instant> {
        use std::sync::mpsc::RecvTimeoutError;
        match self.lines.recv_timeout(timeout) {
            Ok((at, _line)) => Ok(at),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("server closed stdout before {what}")
            }
            Err(RecvTimeoutError::Timeout) => {
                bail!("no {what} within {}s", timeout.as_secs())
            }
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Close stdin first, then kill: the measurement is over either way.
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// MCP cold-start: time from process spawn to the first JSON-RPC response.
///
/// Spawns the server script, sends an MCP `initialize` request via stdin,
/// and records wall-clock time until a JSON-RPC response line arrives on
/// stdout. Kills the server after each measurement.
fn run_mcp_cold_start(rt: &Runtime, server_script: &Path) -> Result<CaseResult> {
    print!("  {}/mcp-cold-start: ", rt.name);
    let iters = COLD_START_ITERS;
    let mut samples = Vec::with_capacity(iters);

    let init_msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "clientInfo": { "name": "bench", "version": "0.0.1" },
            "capabilities": {}
        }
    })
    .to_string()
        + "\n";

    for i in 0..iters {
        let cmd = rt.build_cmd(server_script, &[]);

        let start = Instant::now();
        let mut server = McpServer::spawn(cmd)?;

        // Write the initialize request; the first non-empty stdout line is its
        // response. Stdin stays open until `server` drops (see McpServer).
        server.send(&init_msg)?;
        let answered = server
            .next_line("the initialize response", CASE_TIMEOUT)
            .with_context(|| format!("{}/mcp-cold-start iter {i}", rt.name))?;
        let elapsed = answered.duration_since(start).as_secs_f64() * 1000.0;

        drop(server); // close stdin, kill and reap AFTER reading the response
        samples.push(elapsed);
        print!(".");

        // Brief pause between iterations to let OS reclaim file descriptors.
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(" done");

    let (p50, min, max, p95, p99) = stats(&samples);
    Ok(CaseResult {
        runtime: rt.name.clone(),
        name: "mcp-cold-start".to_string(),
        median: p50,
        min,
        max,
        p50,
        p95,
        p99,
        unit: "ms".to_string(),
        iterations: iters,
        warmup_iterations: 0,
        samples,
    })
}

/// The memory figure, in KB, from `tasklist /FO CSV /NH` output. The one
/// parse `process_rss_bytes` uses on Windows, split out so the tests exercise
/// the production code rather than a copy of it.
///
/// The memory field embeds a thousands separator, so splitting the line on
/// ',' cuts THAT FIELD in half. The previous version did exactly that and
/// rejoined the halves in reverse -- "22,220 K" parsed as 22022 -- so every
/// RSS figure this ever reported on Windows was scrambled, plausibly enough
/// to go unnoticed. Take the last QUOTED field instead, which is
/// separator-agnostic.
#[cfg(target_os = "windows")]
fn parse_tasklist_rss_kb(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.contains('"'))?;
    let field = line.rsplit('"').nth(1)?;
    let digits: String = field.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The `VmRSS` figure, in kB, from the text of `/proc/<pid>/status`. The one
/// parse `process_rss_bytes` uses on Linux, split out (and compiled under
/// `test` on every host) so the tests exercise the production code.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_status_vmrss_kb(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Read RSS of a running process in bytes. Best-effort; returns None on failure.
fn process_rss_bytes(pid: u32) -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        // Query Windows via `tasklist /FI "PID eq <pid>" /FO CSV /NH`
        // Output: "oam.exe","<pid>","Console","1","12,345 K"
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let kb = parse_tasklist_rss_kb(&text)?;
        Some(kb * 1024)
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/status VmRSS line: "VmRSS:   12345 kB"
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let kb = parse_proc_status_vmrss_kb(&status)?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        // macOS (and other non-linux unix): no /proc. Read the child's RSS by
        // PID via sysinfo. sysinfo's Process::memory() already returns bytes.
        use sysinfo::{Pid, ProcessesToUpdate, System};
        let pid = Pid::from_u32(pid);
        let mut sys = System::new();
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        sys.process(pid).map(|p| p.memory())
    }
}

/// MCP idle RSS: spawn server, complete `initialize`, sample RSS, kill.
/// Unit: MB (megabytes). The initialize round-trip ensures the runtime is
/// fully loaded (module graph evaluated, event loop alive) before sampling.
fn run_mcp_idle_rss(rt: &Runtime, server_script: &Path) -> Result<CaseResult> {
    print!("  {}/mcp-idle-rss: ", rt.name);
    let iters = COLD_START_ITERS;
    let mut samples = Vec::with_capacity(iters);

    let init_msg = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25",
                    "clientInfo": { "name": "bench", "version": "0.0.1" },
                    "capabilities": {} }
    })
    .to_string()
        + "\n";

    for i in 0..iters {
        let mut server = McpServer::spawn(rt.build_cmd(server_script, &[]))?;
        let pid = server.child.id();

        // A server that never answers `initialize` is not "idle after
        // initialize": fail the sample rather than publish the RSS of a
        // process in some other state (or wait on it forever).
        server.send(&init_msg)?;
        server
            .next_line("the initialize response", CASE_TIMEOUT)
            .with_context(|| format!("{}/mcp-idle-rss iter {i}", rt.name))?;

        // Server is now idle (waiting for next request) -- sample RSS.
        if let Some(bytes) = process_rss_bytes(pid) {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            samples.push(mb);
        }

        drop(server);
        print!(".");
        std::thread::sleep(Duration::from_millis(100));
    }
    println!(" done");

    if samples.is_empty() {
        bail!("no RSS samples collected (process_rss_bytes returned None on every iter)");
    }

    let (p50, min, max, p95, p99) = stats(&samples);
    Ok(CaseResult {
        runtime: rt.name.clone(),
        name: "mcp-idle-rss".to_string(),
        median: p50,
        min,
        max,
        p50,
        p95,
        p99,
        unit: "MB".to_string(),
        iterations: samples.len(),
        warmup_iterations: 0,
        samples,
    })
}

/// MCP first-call latency: initialize then send a `tools/call` and measure
/// time from send to response. The initialize is NOT included -- this is
/// warm-path latency on an already-running server.
fn run_mcp_first_call_latency(rt: &Runtime, server_script: &Path) -> Result<CaseResult> {
    print!("  {}/mcp-first-call-latency: ", rt.name);
    let iters = COLD_START_ITERS;
    let mut samples = Vec::with_capacity(iters);

    let init_msg = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25",
                    "clientInfo": { "name": "bench", "version": "0.0.1" },
                    "capabilities": {} }
    })
    .to_string()
        + "\n";

    // MCP requires an `initialized` notification after the initialize response.
    let initialized_notif = serde_json::json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    })
    .to_string()
        + "\n";

    let call_msg = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "noop", "arguments": {} }
    })
    .to_string()
        + "\n";

    for i in 0..iters {
        let iter_label = || format!("{}/mcp-first-call-latency iter {i}", rt.name);
        let mut server = McpServer::spawn(rt.build_cmd(server_script, &[]))?;

        // Drain the initialize response.
        server.send(&init_msg)?;
        server
            .next_line("the initialize response", CASE_TIMEOUT)
            .with_context(iter_label)?;

        // Send the initialized notification (required by MCP spec before tools/call).
        server.send(&initialized_notif)?;

        // Now measure: send tools/call, time to response.
        let call_start = Instant::now();
        server.send(&call_msg)?;
        let answered = server
            .next_line("the tools/call response", CASE_TIMEOUT)
            .with_context(iter_label)?;
        let elapsed_ms = answered.duration_since(call_start).as_secs_f64() * 1000.0;
        samples.push(elapsed_ms);

        drop(server);
        print!(".");
        std::thread::sleep(Duration::from_millis(50));
    }
    println!(" done");

    let (p50, min, max, p95, p99) = stats(&samples);
    Ok(CaseResult {
        runtime: rt.name.clone(),
        name: "mcp-first-call-latency".to_string(),
        median: p50,
        min,
        max,
        p50,
        p95,
        p99,
        unit: "ms".to_string(),
        iterations: iters,
        warmup_iterations: 0,
        samples,
    })
}

// ---------------------------------------------------------- ts-cold-start

/// Modules in the generated TypeScript graph, not counting the entry.
const TS_MODULES: usize = 20;
/// The entry imports module 1 and every `TS_FANOUT_STRIDE`th module directly
/// (the fan-out); every module imports the next one (the chain).
const TS_FANOUT_STRIDE: usize = 5;

/// The states a TypeScript load is measured in. Each is a published row, in
/// this order.
///
/// The three oam rows bracket the bytecode cache: `NoCache` is the floor with
/// the cache switched off, `Cold` adds the cost of producing it, `Warm` shows
/// what consuming it saves. When this case was written none of them skipped
/// the `.ts` -> JS transpile itself (only `node_modules` got the install-time
/// precompile), which made `Warm` the row a transpile cache had to move;
/// since the transpile cache shipped (`oam_loader::transpile_cache`,
/// `OAM_TRANSPILE_CACHE`), `Warm` has it warm too. Before this case, nothing
/// measured the TypeScript path in any state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TsState {
    /// `OAM_CODE_CACHE=0` and a fresh `OAM_CACHE_DIR` per run: transpile plus
    /// a full V8 compile, with no bytecode produced, written or consumed.
    NoCache,
    /// Caches on and a fresh `OAM_CACHE_DIR` per run: a project's first run,
    /// which transpiles, compiles, AND serializes + writes the bytecode.
    Cold,
    /// Caches on and one `OAM_CACHE_DIR` primed by an untimed run, then
    /// reused: every run after the first.
    Warm,
    /// The reference runtime as installed, with no cache control.
    Reference,
}

impl TsState {
    const OAM: [TsState; 3] = [TsState::NoCache, TsState::Cold, TsState::Warm];

    fn variant(self) -> &'static str {
        match self {
            TsState::NoCache => "no-cache",
            TsState::Cold => "cold",
            TsState::Warm => "warm",
            TsState::Reference => "reference",
        }
    }
}

/// One published row of the ts-cold-start case.
struct TsRow {
    variant: String,
    result: CaseResult,
}

/// Everything the ts-cold-start case publishes. It has its own section in
/// both output files rather than a line in the main table because its shape
/// is different: several rows per runtime, one per cache state.
struct TsSection {
    /// The node flag that ran the fixture, when a node row exists.
    node_flag: Option<String>,
    rows: Vec<TsRow>,
}

/// Row label as published: `oam no-cache`, `oam cold`, `oam warm`, `node`.
fn ts_row_label(runtime: &str, variant: &str) -> String {
    if variant == TsState::Reference.variant() {
        runtime.to_string()
    } else {
        format!("{runtime} {variant}")
    }
}

struct TsFixture {
    entry: PathBuf,
}

/// One module of the graph. `@N@` is the zero-padded module number, `@I@` the
/// plain one; the `@CHILD_*@` slots link to the next module and are empty on
/// the last. The syntax is chosen to exercise a transpiler, not a type checker:
/// interfaces and `import type` (erased), an enum (lowered -- non-erasable,
/// which is why node needs transform mode for it), generics on inner
/// functions, and enough statements per module that the compile is not
/// trivially empty.
const TS_MODULE_TEMPLATE: &str = r#"// mod@N@.ts -- generated by `cargo xtask bench` (ts-cold-start fixture). Do not edit.

@IMPORTS@
export interface Record@N@ {
  id: number;
  label: string;
  tags: readonly string[];
@CHILD_FIELD@}

export enum Stage@N@ {
  Idle = 0,
  Parsing = 1,
  Bound = 2,
  Emitted = 3,
}

export type Pair@N@<A, B> = { first: A; second: B };

function scale@N@<T extends { id: number }>(value: T, factor: number): number {
  return value.id * factor;
}

function describe@N@(record: Record@N@): string {
  const stage: Stage@N@ = record.id % 2 === 0 ? Stage@N@.Bound : Stage@N@.Parsing;
  return `${record.label}:${stage}:${record.tags.length}`;
}

function combine@N@<K extends string, V>(entries: ReadonlyArray<Pair@N@<K, V>>): Map<K, V> {
  const out = new Map<K, V>();
  for (const { first, second } of entries) {
    out.set(first, second);
  }
  return out;
}

export function make@N@(id: number): Record@N@ {
  return { id, label: 'mod@N@', tags: ['a', 'b', 'c']@CHILD_INIT@ };
}

export function run@N@(seed: number): number {
  const record = make@N@(seed);
  const pairs = combine@N@([
    { first: 'x', second: seed },
    { first: 'y', second: seed + 1 },
  ]);
  return scale@N@(record, @I@) + describe@N@(record).length + pairs.size@CHILD_CALL@;
}
"#;

/// The entry. Its first stdout line is what the case times to, and every row
/// must print the same one -- a runtime that lowered the enums differently
/// would disagree on the total, and the harness treats that as a failure
/// rather than a timing.
const TS_ENTRY_TEMPLATE: &str = r#"// main.ts -- generated by `cargo xtask bench` (ts-cold-start fixture). Do not edit.

@IMPORTS@
interface Summary {
  total: number;
  parts: readonly number[];
}

function summarize(parts: readonly number[]): Summary {
  let total = 0;
  for (const part of parts) {
    total += part;
  }
  return { total, parts };
}

const probe: Record01 = make01(0);
const summary: Summary = summarize([probe.tags.length, @CALLS@]);
console.log(`ok ${summary.total} ${summary.parts.length}`);
"#;

fn ts_module_source(i: usize, next: Option<usize>) -> String {
    let (imports, child_field, child_init, child_call) = match next {
        Some(j) => {
            let nn = format!("{j:02}");
            (
                format!(
                    "import {{ make{nn}, run{nn} }} from './mod{nn}.ts';\n\
                     import type {{ Record{nn} }} from './mod{nn}.ts';\n\n"
                ),
                format!("  child?: Record{nn};\n"),
                format!(", child: make{nn}(id + 1)"),
                format!(" + run{nn}(seed + 1)"),
            )
        }
        None => Default::default(),
    };
    TS_MODULE_TEMPLATE
        .replace("@IMPORTS@\n", &imports)
        .replace("@CHILD_FIELD@", &child_field)
        .replace("@CHILD_INIT@", &child_init)
        .replace("@CHILD_CALL@", &child_call)
        .replace("@I@", &i.to_string())
        .replace("@N@", &format!("{i:02}"))
}

fn ts_fanout() -> impl Iterator<Item = usize> {
    (1..=TS_MODULES).filter(|i| *i == 1 || i.is_multiple_of(TS_FANOUT_STRIDE))
}

fn ts_entry_source() -> String {
    let mut imports = String::from(
        "import { make01, run01 } from './mod01.ts';\nimport type { Record01 } from './mod01.ts';\n",
    );
    for i in ts_fanout().skip(1) {
        imports.push_str(&format!("import {{ run{i:02} }} from './mod{i:02}.ts';\n"));
    }
    imports.push('\n');
    let calls: Vec<String> = ts_fanout().map(|i| format!("run{i:02}({i})")).collect();
    TS_ENTRY_TEMPLATE
        .replace("@IMPORTS@\n", &imports)
        .replace("@CALLS@", &calls.join(", "))
}

/// Generate the module graph under `dir`. Byte-for-byte deterministic, so the
/// bytecode cache keys are stable within a run and two runs' fixtures diff
/// clean. Generated at bench time like every other script here; nothing in
/// it is slow enough to be worth committing.
fn write_ts_fixture(dir: &Path) -> Result<TsFixture> {
    std::fs::create_dir_all(dir)?;
    // node reads the module format of a `.ts` file from the nearest
    // package.json; oam infers it from the source. Say it once so they agree.
    std::fs::write(
        dir.join("package.json"),
        "{\"name\":\"ts-cold-start-fixture\",\"private\":true,\"type\":\"module\"}\n",
    )?;
    for i in 1..=TS_MODULES {
        let next = (i < TS_MODULES).then_some(i + 1);
        std::fs::write(dir.join(format!("mod{i:02}.ts")), ts_module_source(i, next))?;
    }
    let entry = dir.join("main.ts");
    std::fs::write(&entry, ts_entry_source())?;
    Ok(TsFixture { entry })
}

/// The command for one row. Every cache variable is set explicitly rather
/// than inherited: the row label promises a state, and nothing in the
/// harness's own environment may be able to change it.
///
/// That includes `OAM_TRANSPILE_CACHE`, pinned ON for all three oam rows (its
/// default). It used to be inherited, so a shell exporting
/// `OAM_TRANSPILE_CACHE=0` made the `warm` row re-run oxc on every sample
/// under a legend saying both caches are warm -- and the `.v8c` count cannot
/// see that. The rows differ in the BYTECODE cache and in whether the cache
/// directory is fresh; a fresh directory is what makes the transpile cache
/// cold on `no-cache` and `cold`.
fn ts_cmd(
    rt: &Runtime,
    state: TsState,
    entry: &Path,
    cache_dir: &Path,
    node_flag: Option<&str>,
) -> Command {
    let mut cmd = Command::new(&rt.exe);
    if state == TsState::Reference {
        if let Some(flag) = node_flag {
            cmd.arg(flag);
        }
        cmd.arg(entry);
        // Node's compile cache is opt-in via this variable; the row is node as
        // installed, so make sure the environment has not opted in.
        cmd.env_remove("NODE_COMPILE_CACHE");
    } else {
        cmd.arg("run").arg(entry).arg("--no-check");
        cmd.env("OAM_CACHE_DIR", cache_dir);
        cmd.env(
            "OAM_CODE_CACHE",
            if state == TsState::NoCache { "0" } else { "1" },
        );
        cmd.env("OAM_TRANSPILE_CACHE", TS_TRANSPILE_CACHE);
    }
    cmd
}

/// What `ts_cmd` pins `OAM_TRANSPILE_CACHE` to on every oam row, and what the
/// section records as `transpile_cache` so a reader (and `push_ts_markdown`)
/// can tell a pinned measurement from one taken before the pin existed.
const TS_TRANSPILE_CACHE: &str = "1";

/// The line of a stderr dump worth quoting: the first one naming an error,
/// else the first non-empty one. Node prints the offending source and a caret
/// before the `SyntaxError [ERR_...]` line, so "first line" alone is a path.
fn error_line(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .find(|l| l.contains("Error"))
        .or(lines.first())
        .map(|l| l.chars().take(200).collect())
        .unwrap_or_default()
}

/// Which of node's two TypeScript modes runs the fixture. Strip-only mode
/// (`--experimental-strip-types`) erases annotations but rejects anything it
/// would have to lower -- the fixture's enums included -- with
/// `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`; transform mode lowers them. Probed
/// against the real fixture, cheapest mode first, so the row says what it
/// measured instead of assuming a flag that may not exist on the installed
/// node. `Err` carries the reason for the console line.
fn probe_node_ts_flag(node: &Runtime, entry: &Path) -> std::result::Result<String, String> {
    let mut why = String::new();
    for flag in [
        "--experimental-strip-types",
        "--experimental-transform-types",
    ] {
        let mut cmd = Command::new(&node.exe);
        cmd.arg(flag).arg(entry).env_remove("NODE_COMPILE_CACHE");
        match run_with_timeout(&mut cmd, CASE_TIMEOUT) {
            Ok(out) if out.code == 0 && out.stdout.starts_with("ok ") => {
                return Ok(flag.to_string());
            }
            Ok(out) => {
                why = format!("{flag}: exit {} ({})", out.code, error_line(&out.stderr));
            }
            Err(e) => why = format!("{flag}: {e}"),
        }
    }
    Err(why)
}

/// Wall-clock from process spawn to its first stdout line, then wait for a
/// clean exit. Returns the elapsed milliseconds and that line.
///
/// The clock stops on the reader thread, right after `read_line` returns, so
/// the figure carries no harness scheduling between the line landing in the
/// pipe and the main thread noticing. The pipe is drained after the first
/// line and stderr is drained throughout, so a chatty child cannot block on a
/// full pipe and inflate its own number.
fn time_to_first_line(cmd: &mut Command, timeout: Duration) -> Result<(f64, String)> {
    use std::io::{BufRead, Read};
    use std::sync::mpsc;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().with_context(|| format!("spawning {cmd:?}"))?;
    let stdout = child.stdout.take().context("stdout piped")?;
    let mut stderr = child.stderr.take().context("stderr piped")?;

    let (tx, rx) = mpsc::channel();
    let stdout_thread = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let first = match reader.read_line(&mut line) {
            Ok(n) if n > 0 => Some((start.elapsed(), line)),
            _ => None,
        };
        let _ = tx.send(first);
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    let first = match rx.recv_timeout(timeout) {
        Ok(first) => first,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("no stdout line within {}s", timeout.as_secs());
        }
    };

    let deadline = start + timeout;
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code().unwrap_or(-1);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("did not exit within {}s of spawn", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let _ = stdout_thread.join();
    let stderr = stderr_thread.join().unwrap_or_default();

    let Some((elapsed, line)) = first else {
        bail!(
            "exit {code} before any stdout line; stderr: {}",
            error_line(&stderr)
        );
    };
    if code != 0 {
        bail!("exit {code}; stderr: {}", error_line(&stderr));
    }
    Ok((elapsed.as_secs_f64() * 1000.0, line.trim_end().to_string()))
}

/// Bytecode blobs under a cache root. The row labels are load-bearing, and
/// this is how the harness checks them: a `no-cache` run must leave none, a
/// `cold` run must leave some, a `warm` prime must have left some to consume.
fn count_v8c(dir: &Path) -> usize {
    fn walk(dir: &Path, n: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, n);
            } else if path.extension().is_some_and(|e| e == "v8c") {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}

/// What every sample of the case shares.
struct TsRun<'a> {
    entry: &'a Path,
    /// Per-sample fresh cache directories are created and removed under here.
    tmp: &'a Path,
    /// The one primed directory every `Warm` sample reuses.
    warm_dir: &'a Path,
    node_flag: Option<&'a str>,
}

/// One timed run of one row. `expected` is the first line every row has to
/// agree on, set by whichever row ran first.
fn ts_sample_once(
    run: &TsRun<'_>,
    rt: &Runtime,
    state: TsState,
    i: usize,
    expected: &mut Option<String>,
) -> Result<f64> {
    let label = format!("{}/{}", rt.name, state.variant());
    let fresh = matches!(state, TsState::NoCache | TsState::Cold);
    let cache_dir = if fresh {
        run.tmp.join(format!("ts-cache-{}-{i}", state.variant()))
    } else {
        run.warm_dir.to_path_buf()
    };
    if fresh {
        std::fs::create_dir_all(&cache_dir)?;
    }

    let outcome = time_to_first_line(
        &mut ts_cmd(rt, state, run.entry, &cache_dir, run.node_flag),
        CASE_TIMEOUT,
    );
    // Inspect, then discard, the per-run cache before deciding anything else,
    // so a failed run does not leave its directory behind.
    let blobs = fresh.then(|| {
        let n = count_v8c(&cache_dir);
        let _ = std::fs::remove_dir_all(&cache_dir);
        n
    });

    let (ms, line) = outcome.with_context(|| format!("{label}: iteration {i}"))?;
    if !line.starts_with("ok ") {
        bail!("{label}: iteration {i} printed {line:?}, not the fixture's `ok ...` line");
    }
    match expected {
        Some(agreed) if *agreed != line => bail!(
            "{label}: iteration {i} printed {line:?} where an earlier row printed {agreed:?} \
             -- the runtimes disagree on what the fixture computes"
        ),
        Some(_) => {}
        None => *expected = Some(line),
    }
    match (state, blobs) {
        (TsState::NoCache, Some(n)) if n > 0 => bail!(
            "{label}: iteration {i} wrote {n} .v8c blob(s) under OAM_CODE_CACHE=0 -- the off \
             switch did not take, so this is not a no-cache measurement"
        ),
        (TsState::Cold, Some(0)) => bail!(
            "{label}: iteration {i} wrote no .v8c blobs -- the cache is not producing, so this \
             row would be a no-cache row in disguise"
        ),
        _ => {}
    }
    Ok(ms)
}

/// ts-cold-start: spawn to first stdout line of the generated TypeScript
/// graph, one row per cache state for oam plus node as installed, sampled
/// round-robin across the rows like `run_timed_case_interleaved` so drift
/// lands on every row rather than on whichever ran last.
fn run_ts_cold_start(runtimes: &[Runtime], fixture: &TsFixture, tmp: &Path) -> Result<TsSection> {
    let mut plan: Vec<(&Runtime, TsState)> = Vec::new();
    let mut node_flag: Option<String> = None;
    for rt in runtimes {
        match rt.name.as_str() {
            "oam" => plan.extend(TsState::OAM.iter().map(|s| (rt, *s))),
            "node" => match probe_node_ts_flag(rt, &fixture.entry) {
                Ok(flag) => {
                    println!("  node: runs the fixture with {flag}");
                    node_flag = Some(flag);
                    plan.push((rt, TsState::Reference));
                }
                Err(why) => println!("  node/{TS_CASE}: skipped -- {why}"),
            },
            other => println!(
                "  {other}/{TS_CASE}: skipped (this case measures oam per cache state, with node as the reference)"
            ),
        }
    }
    if plan.is_empty() {
        bail!("no runtime to measure");
    }

    // Prime the warm cache once, untimed. Every warm sample then consumes
    // what this run produced.
    let warm_dir = tmp.join("ts-cache-warm");
    if plan.iter().any(|(_, s)| *s == TsState::Warm) {
        let oam = runtimes
            .iter()
            .find(|r| r.name == "oam")
            .context("oam runtime")?;
        std::fs::create_dir_all(&warm_dir)?;
        time_to_first_line(
            &mut ts_cmd(oam, TsState::Warm, &fixture.entry, &warm_dir, None),
            CASE_TIMEOUT,
        )
        .context("priming the warm cache")?;
        let blobs = count_v8c(&warm_dir);
        if blobs == 0 {
            bail!("the warm-cache prime wrote no .v8c blobs, so the warm row would be a cold row");
        }
    }

    let iters = COLD_START_ITERS;
    let mut samples: Vec<Vec<f64>> = vec![Vec::with_capacity(iters); plan.len()];
    let mut failed: Vec<Option<String>> = vec![None; plan.len()];
    let mut expected: Option<String> = None;

    let labels: Vec<String> = plan
        .iter()
        .map(|(rt, s)| format!("{}:{}", rt.name, s.variant()))
        .collect();
    print!("  {TS_CASE} [{}]: ", labels.join(" "));
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let run = TsRun {
        entry: &fixture.entry,
        tmp,
        warm_dir: &warm_dir,
        node_flag: node_flag.as_deref(),
    };
    for i in 0..iters {
        for (idx, (rt, state)) in plan.iter().enumerate() {
            if failed[idx].is_some() {
                continue;
            }
            match ts_sample_once(&run, rt, *state, i, &mut expected) {
                Ok(v) => samples[idx].push(v),
                Err(e) => failed[idx] = Some(format!("{e:#}")),
            }
        }
        print!(".");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    println!(" done");
    let _ = std::fs::remove_dir_all(&warm_dir);

    let mut rows = Vec::new();
    for (idx, (rt, state)) in plan.iter().enumerate() {
        if let Some(err) = &failed[idx] {
            println!(
                "  {}/{TS_CASE} {}: FAILED -- {err}",
                rt.name,
                state.variant()
            );
            continue;
        }
        let s = std::mem::take(&mut samples[idx]);
        let (p50, min, max, p95, p99) = stats(&s);
        rows.push(TsRow {
            variant: state.variant().to_string(),
            result: CaseResult {
                runtime: rt.name.clone(),
                name: TS_CASE.to_string(),
                median: p50,
                min,
                max,
                p50,
                p95,
                p99,
                unit: "ms".to_string(),
                iterations: s.len(),
                // The prime is the one run whose figure is not published.
                warmup_iterations: usize::from(*state == TsState::Warm),
                samples: s,
            },
        });
    }
    if rows.is_empty() {
        bail!("every row failed");
    }
    // A node row with no oam rows to compare against is a reference to
    // nothing; still worth publishing, but say so.
    if !rows.iter().any(|r| r.result.runtime == "oam") {
        println!("  note: no oam row survived; the section has only the reference");
    }
    Ok(TsSection { node_flag, rows })
}

// ------------------------------------------------------------------ entry

pub fn run(release: bool, compare: bool, only: &[String]) -> Result<()> {
    let repo = repo_root()?;

    // Resolved FIRST -- before the build, the staging copy and the temp dir --
    // so a typo fails in under a second and leaves nothing behind. (It used
    // to sit below all three while claiming this; `ensure_oam_built` always
    // invokes cargo.)
    let selected = select_cases(only)?;
    let filtered = !only.is_empty();
    let results_path = repo.join("bench").join("results.json");
    // A filtered run merges into the committed file; refuse up front if there
    // is nothing to merge into, rather than after the measurements.
    let existing: Option<serde_json::Value> = if filtered {
        let text = std::fs::read_to_string(&results_path).with_context(|| {
            format!(
                "--case merges into {}, which is missing -- run once without --case first",
                results_path.display()
            )
        })?;
        Some(serde_json::from_str(&text).context("parsing bench/results.json")?)
    } else {
        None
    };

    let oam = ensure_oam_built(&repo, release)?;
    // Time a file nothing else is writing to, not a build artifact `cargo` may
    // replace mid-run. See stage_for_benchmark.
    // `_staged` must stay alive for the whole run: dropping it deletes the copy.
    let (oam, _staged) = match stage_for_benchmark(&oam) {
        Ok((path, guard)) => (path, Some(guard)),
        Err(err) => {
            eprintln!(
                "  warning: could not stage the oam binary out of target/ ({err}).\n  \
                 Timings may include per-exec virus-scanner overhead that node/bun do not pay."
            );
            (oam, None)
        }
    };
    let profile = if release { "release" } else { "debug" };
    let oam_version = capture_version(&oam, &["--version"]);
    let commit = git_short_commit(&repo);

    let runtimes = if compare {
        discover_runtimes(oam, &oam_version)
    } else {
        vec![Runtime {
            name: "oam".to_string(),
            exe: oam,
            version: oam_version.clone(),
        }]
    };

    println!("oam bench @ {commit} ({profile})\n");
    for rt in &runtimes {
        println!("  {} {}", rt.name, rt.version);
    }
    println!();

    let tmp = std::env::temp_dir().join(format!("oam-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    // Removed on every exit from here on, an early `?` included.
    let _tmp_guard = StagedBinary { dir: tmp.clone() };

    let scripts = write_scripts(&tmp, &repo)?;
    let ts_fixture = write_ts_fixture(&tmp.join("ts-fixture"))?;
    let mut all_results: Vec<CaseResult> = Vec::new();
    let mut ts_section: Option<TsSection> = None;

    for case_name in &selected {
        println!("{case_name}:");

        if *case_name == TS_CASE {
            match run_ts_cold_start(&runtimes, &ts_fixture, &tmp) {
                Ok(section) => ts_section = Some(section),
                Err(e) => println!("  {TS_CASE}: FAILED -- {e:#}"),
            }
            continue;
        }

        // The script-driven timed cases sample every runtime round-robin, so
        // background load cannot bias one runtime's block relative to another's.
        // The remaining cases (cold-start and the mcp-* trio) drive a runtime
        // differently per runtime and keep the per-runtime path below.
        let timed: Option<(&Path, usize, Vec<String>)> = match *case_name {
            "url-parse" => Some((scripts.url_parse.as_path(), TIMED_ITERS, vec![])),
            "http-throughput" => Some((scripts.http_throughput.as_path(), HTTP_ITERS, vec![])),
            "http-keepalive-latency" => {
                Some((scripts.http_keepalive_latency.as_path(), HTTP_ITERS, vec![]))
            }
            "fs-read" => Some((
                scripts.fs_read.as_path(),
                TIMED_ITERS,
                vec![scripts.data_file.to_string_lossy().to_string()],
            )),
            "json-parse" => Some((scripts.json_parse.as_path(), TIMED_ITERS, vec![])),
            "crypto-hash" => Some((scripts.crypto_hash.as_path(), TIMED_ITERS, vec![])),
            _ => None,
        };
        if let Some((script, iters, extra)) = timed {
            for (rt_name, result) in
                run_timed_case_interleaved(case_name, &runtimes, script, iters, &extra)
            {
                match result {
                    Ok(r) => all_results.push(r),
                    Err(e) => println!("  {rt_name}/{case_name}: FAILED -- {e}"),
                }
            }
            continue;
        }

        for rt in &runtimes {
            let result = match *case_name {
                "cold-start" => run_cold_start(rt, &scripts.cold_start),
                "mcp-cold-start" => {
                    if rt.name == "oam" {
                        run_mcp_cold_start(rt, &scripts.mcp_server)
                    } else if let Some(sdk) = &scripts.sdk_mcp_server {
                        run_mcp_cold_start(rt, sdk)
                    } else {
                        println!("  {}/mcp-cold-start: skipped (SDK install failed)", rt.name);
                        continue;
                    }
                }
                "mcp-idle-rss" => {
                    if rt.name == "oam" {
                        run_mcp_idle_rss(rt, &scripts.mcp_server)
                    } else if let Some(sdk) = &scripts.sdk_mcp_server {
                        run_mcp_idle_rss(rt, sdk)
                    } else {
                        println!("  {}/mcp-idle-rss: skipped (SDK install failed)", rt.name);
                        continue;
                    }
                }
                "mcp-first-call-latency" => {
                    if rt.name == "oam" {
                        run_mcp_first_call_latency(rt, &scripts.mcp_server)
                    } else if let Some(sdk) = &scripts.sdk_mcp_server {
                        run_mcp_first_call_latency(rt, sdk)
                    } else {
                        println!(
                            "  {}/mcp-first-call-latency: skipped (SDK install failed)",
                            rt.name
                        );
                        continue;
                    }
                }
                _ => unreachable!(),
            };
            match result {
                Ok(r) => all_results.push(r),
                Err(e) => {
                    // `{e:#}`: the mcp-* errors carry the iteration as context
                    // and the reason (timeout, closed pipe) as the cause.
                    println!("  {}/{case_name}: FAILED -- {e:#}", rt.name);
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);

    // ------------------------------------------------------------- table
    let main_selected: Vec<&str> = selected.iter().copied().filter(|c| *c != TS_CASE).collect();
    if !main_selected.is_empty() {
        print_table(&all_results, &runtimes, &main_selected);
    }
    if let Some(ts) = &ts_section {
        print_ts_table(ts);
    }

    // --------------------------------------------------------- write files
    // BENCHMARKS.md is derived from the JSON, never from this run's structs
    // directly, so a merged file and a fresh one go through the same code.
    let fresh = build_json(
        &all_results,
        &runtimes,
        ts_section.as_ref(),
        &commit,
        profile,
    );
    let json = match existing {
        Some(existing) => {
            let measured: Vec<&str> = runtimes.iter().map(|r| r.name.as_str()).collect();
            merge_filtered_run(existing, fresh, &selected, &measured)?
        }
        None => fresh,
    };
    std::fs::create_dir_all(repo.join("bench"))?;
    std::fs::write(&results_path, serde_json::to_string_pretty(&json)?)?;
    std::fs::write(repo.join("BENCHMARKS.md"), build_markdown(&json))?;

    if filtered {
        println!(
            "merged {} into BENCHMARKS.md + bench/results.json",
            selected.join(", ")
        );
    } else {
        println!("wrote BENCHMARKS.md + bench/results.json");
    }
    Ok(())
}

/// Fold a `--case` run into the committed results.
///
/// The rows this run measured -- every selected case, for every runtime that
/// took part -- are replaced, whether or not they succeeded: a case that
/// failed shows as missing rather than as last month's number wearing today's
/// label. Everything else is kept verbatim, including the top-level stamp,
/// which keeps describing the last full run; each fresh row carries its own
/// `commit` so the markdown can say which rows are from a different tree.
/// The TypeScript section is replaced whole when it was measured and kept
/// when it was not, since it stamps itself.
///
/// Profile and host must match: a debug row in a release table, or a mac row
/// in a windows one, is a mixture the single header line cannot express.
fn merge_filtered_run(
    mut doc: serde_json::Value,
    fresh: serde_json::Value,
    selected: &[&str],
    measured_runtimes: &[&str],
) -> Result<serde_json::Value> {
    for key in ["profile", "host"] {
        if doc[key] != fresh[key] {
            bail!(
                "bench/results.json was measured with {key} {} and this run is {key} {} -- \
                 a filtered run cannot merge across that; run without --case to replace the file",
                doc[key],
                fresh[key]
            );
        }
    }

    let field = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let mut rows: Vec<serde_json::Value> = doc["results"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            !(selected.contains(&field(r, "case").as_str())
                && measured_runtimes.contains(&field(r, "runtime").as_str()))
        })
        .collect();
    // A fresh row carries the version of the binary that measured it, beside
    // its commit stamp: the runtimes list keeps describing the full run
    // behind every other row.
    let fresh_runtimes = fresh["runtimes"].as_array().cloned().unwrap_or_default();
    rows.extend(
        fresh["results"]
            .as_array()
            .iter()
            .flat_map(|a| a.iter().cloned())
            .map(|mut row| {
                if let Some(rt) = fresh_runtimes
                    .iter()
                    .find(|rt| rt["name"] == row["runtime"])
                {
                    row["version"] = rt["version"].clone();
                }
                row
            }),
    );

    // Runtimes: the list names the binaries behind the main table's rows,
    // and most of them are the last full run's, so an entry is never
    // replaced -- re-measured rows say their own version. A runtime the
    // full run did not have joins when this run put a row under it; the
    // TypeScript section stamps its own.
    let mut runtimes: Vec<serde_json::Value> =
        doc["runtimes"].as_array().cloned().unwrap_or_default();
    let fresh_main_rows = fresh["results"].as_array().cloned().unwrap_or_default();
    for rt in fresh_runtimes {
        if fresh_main_rows.iter().any(|r| r["runtime"] == rt["name"])
            && !runtimes.iter().any(|r| r["name"] == rt["name"])
        {
            runtimes.push(rt);
        }
    }

    // Table order: by case, then by runtime column, so the merged file diffs
    // like a fresh one rather than growing an appendix.
    let case_index = |r: &serde_json::Value| {
        CASE_NAMES
            .iter()
            .position(|c| *c == field(r, "case"))
            .unwrap_or(usize::MAX)
    };
    let rt_index = |r: &serde_json::Value| {
        runtimes
            .iter()
            .position(|rt| rt["name"] == r["runtime"])
            .unwrap_or(usize::MAX)
    };
    rows.sort_by_key(|r| (case_index(r), rt_index(r)));

    doc["schema"] = fresh["schema"].clone();
    doc["results"] = serde_json::Value::Array(rows);
    doc["runtimes"] = serde_json::Value::Array(runtimes);
    if selected.contains(&TS_CASE) && !fresh["ts_cold_start"].is_null() {
        doc["ts_cold_start"] = fresh["ts_cold_start"].clone();
    }
    Ok(doc)
}

// ------------------------------------------------------------ scripts

struct Scripts {
    cold_start: PathBuf,
    url_parse: PathBuf,
    http_throughput: PathBuf,
    http_keepalive_latency: PathBuf,
    fs_read: PathBuf,
    json_parse: PathBuf,
    crypto_hash: PathBuf,
    data_file: PathBuf,
    mcp_server: PathBuf,
    /// SDK server script for node/bun/deno comparison. None if npm install failed.
    sdk_mcp_server: Option<PathBuf>,
}

fn write_scripts(tmp: &Path, repo: &Path) -> Result<Scripts> {
    let cold_start = tmp.join("bench_cold_start.mjs");
    std::fs::write(&cold_start, "console.log('ok');\n")?;

    let url_parse = tmp.join("bench_url_parse.mjs");
    std::fs::write(
        &url_parse,
        r#"const N = 10000;
const urls = [
    'https://user:pass@example.com:8080/path/to/resource?q=1&r=2#frag',
    'http://localhost:3000/api/v2/users',
    'https://subdomain.example.org/a/b/c?x=hello%20world',
    'ftp://files.example.net/pub/docs/rfc.txt',
    'https://example.com',
];
const t0 = performance.now();
for (let i = 0; i < N; i++) {
    new URL(urls[i % urls.length]);
}
const elapsed = performance.now() - t0;
console.log(JSON.stringify({ elapsed_ms: elapsed }));
"#,
    )?;

    let http_throughput = tmp.join("bench_http_throughput.mjs");
    std::fs::write(
        &http_throughput,
        r#"import http from 'node:http';
const N = 200;
const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('ok');
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;
const base = `http://127.0.0.1:${port}`;
const t0 = performance.now();
for (let i = 0; i < N; i++) {
    const res = await fetch(`${base}/bench`);
    await res.text();
}
const elapsed = performance.now() - t0;
server.close();
console.log(JSON.stringify({ elapsed_ms: elapsed, requests: N, rps: (N / elapsed) * 1000 }));
"#,
    )?;

    // #183's loop: the per-request cost of a small fetch on a warm pooled
    // connection -- the path every keep-alive client takes, where a
    // regression in the transport's per-request work shows rather than
    // drowning in connection setup. 50 warm-up fetches, then 10 batches of
    // 200 sequential fetches of a 2-byte body; `elapsed_ms` is the median
    // batch's milliseconds PER REQUEST (min and every batch alongside).
    let http_keepalive_latency = tmp.join("bench_http_keepalive_latency.mjs");
    std::fs::write(
        &http_keepalive_latency,
        r#"import http from 'node:http';
const WARMUP = 50;
const BATCHES = 10;
const PER_BATCH = 200;
const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('ok');
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${server.address().port}/`;
const get = async () => {
    const res = await fetch(url);
    if ((await res.text()) !== 'ok') throw new Error('unexpected body');
};
for (let i = 0; i < WARMUP; i++) await get();
const perRequest = [];
for (let b = 0; b < BATCHES; b++) {
    const t0 = performance.now();
    for (let i = 0; i < PER_BATCH; i++) await get();
    perRequest.push((performance.now() - t0) / PER_BATCH);
}
server.closeAllConnections?.();
server.close();
const sorted = [...perRequest].sort((a, b) => a - b);
const median = (sorted[BATCHES / 2 - 1] + sorted[BATCHES / 2]) / 2;
console.log(JSON.stringify({ elapsed_ms: median, min_ms: sorted[0], batches_ms: perRequest }));
"#,
    )?;

    let fs_read = tmp.join("bench_fs_read.mjs");
    std::fs::write(
        &fs_read,
        r#"import fs from 'node:fs';
const filePath = process.argv[process.argv.length - 1];
const N = 1000;
const t0 = performance.now();
for (let i = 0; i < N; i++) {
    fs.readFileSync(filePath, 'utf8');
}
const elapsed = performance.now() - t0;
console.log(JSON.stringify({ elapsed_ms: elapsed, reads: N }));
"#,
    )?;

    let json_parse = tmp.join("bench_json_parse.mjs");
    std::fs::write(
        &json_parse,
        r#"const N = 1000;
const obj = { users: Array.from({ length: 100 }, (_, i) => ({
    id: i, name: `user_${i}`, email: `user${i}@example.com`,
    roles: ['admin', 'viewer'], meta: { created: '2026-01-01', active: true }
})) };
const text = JSON.stringify(obj);
const t0 = performance.now();
for (let i = 0; i < N; i++) {
    JSON.stringify(JSON.parse(text));
}
const elapsed = performance.now() - t0;
console.log(JSON.stringify({ elapsed_ms: elapsed, ops: N, bytes: text.length }));
"#,
    )?;

    let crypto_hash = tmp.join("bench_crypto_hash.mjs");
    std::fs::write(
        &crypto_hash,
        r#"import crypto from 'node:crypto';
const N = 1000;
const data = Buffer.alloc(65536, 0x42);
const t0 = performance.now();
for (let i = 0; i < N; i++) {
    crypto.createHash('sha256').update(data).digest();
}
const elapsed = performance.now() - t0;
console.log(JSON.stringify({ elapsed_ms: elapsed, ops: N, bytes_per_op: data.length }));
"#,
    )?;

    let data_file = tmp.join("bench_data.txt");
    std::fs::write(&data_file, "x".repeat(4096))?;

    // MCP cold-start server: a minimal oam:mcp server with one tool.
    // The bench spawns this, sends initialize, and measures time to response.
    let mcp_server = tmp.join("bench_mcp_server.mjs");
    std::fs::write(
        &mcp_server,
        r#"import { McpServer } from 'oam:mcp';
const server = new McpServer({ name: 'bench', version: '0.0.1' });
server.tool('noop', {
  description: 'No-op benchmark tool',
  parameters: {},
  handler: async () => ({ content: [{ type: 'text', text: 'ok' }] }),
});
server.serve({ transport: 'stdio' });
"#,
    )?;

    // SDK MCP server: equivalent server using @modelcontextprotocol/sdk for
    // --compare runs. We install the SDK into bench/sdk-fixtures/ (cached across
    // runs -- stable path avoids Defender scan churn during the bench itself).
    let sdk_mcp_server = build_sdk_mcp_server(&repo.join("bench").join("sdk-fixtures"));

    Ok(Scripts {
        cold_start,
        url_parse,
        http_throughput,
        http_keepalive_latency,
        fs_read,
        json_parse,
        crypto_hash,
        data_file,
        mcp_server,
        sdk_mcp_server,
    })
}

/// Ensures @modelcontextprotocol/sdk is installed in `fixtures_dir` and
/// returns the path to the server script. The fixtures dir is stable across
/// runs (bench/sdk-fixtures/) so the install only happens once -- avoids
/// Windows Defender scan churn polluting the benchmark measurements.
fn build_sdk_mcp_server(fixtures: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(fixtures).ok()?;

    let server = fixtures.join("bench_sdk_server.mjs");

    // Always (re)write the script so changes to the fixture are picked up.
    std::fs::write(
        &server,
        r#"import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { ListToolsRequestSchema, CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js';

const server = new Server({ name: 'bench', version: '0.0.1' }, { capabilities: { tools: {} } });

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [{ name: 'noop', description: 'No-op benchmark tool', inputSchema: { type: 'object', properties: {} } }],
}));

server.setRequestHandler(CallToolRequestSchema, async () => ({
  content: [{ type: 'text', text: 'ok' }],
}));

const transport = new StdioServerTransport();
await server.connect(transport);
"#,
    )
    .ok()?;

    // Skip npm install if the SDK is already present.
    let sdk_marker = fixtures
        .join("node_modules")
        .join("@modelcontextprotocol")
        .join("sdk")
        .join("package.json");
    if sdk_marker.exists() {
        return Some(server);
    }

    // Minimal package.json so npm install works without warnings.
    std::fs::write(
        fixtures.join("package.json"),
        r#"{"name":"bench-sdk-fixtures","version":"0.0.0","private":true,"type":"module"}"#,
    )
    .ok()?;

    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };

    print!("  sdk-fixtures: npm install @modelcontextprotocol/sdk ... ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let status = Command::new(npm)
        .args(["install", "--save-exact", "@modelcontextprotocol/sdk"])
        .current_dir(fixtures)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        println!("FAILED (skipping SDK leg)");
        return None;
    }
    println!("ok");

    Some(server)
}

// ------------------------------------------------------------ table output

fn print_table(results: &[CaseResult], runtimes: &[Runtime], case_names: &[&str]) {
    println!();
    if runtimes.len() == 1 {
        println!(
            "{:<20} {:>10} {:>10} {:>10} {:>6}",
            "case", "median", "min", "max", "unit"
        );
        println!("{}", "-".repeat(60));
        for r in results {
            println!(
                "{:<20} {:>10.2} {:>10.2} {:>10.2} {:>6}",
                r.name, r.median, r.min, r.max, r.unit
            );
        }
    } else {
        let rt_names: Vec<&str> = runtimes.iter().map(|r| r.name.as_str()).collect();
        print!("{:<20}", "case");
        for name in &rt_names {
            print!(" {:>10}", name);
        }
        if rt_names.len() >= 2 {
            print!(" {:>8}", "ratio");
        }
        println!();
        println!("{}", "-".repeat(20 + rt_names.len() * 11 + 9));

        for case_name in case_names {
            print!("{:<20}", case_name);
            let mut medians: Vec<Option<f64>> = Vec::new();
            for rt in runtimes {
                let result = results
                    .iter()
                    .find(|r| r.name == *case_name && r.runtime == rt.name);
                match result {
                    Some(r) => {
                        // The row's own unit: mcp-idle-rss is MB, and a
                        // literal "ms" here printed megabytes as a time.
                        let cell = format!("{:.2}{}", r.median, r.unit);
                        print!(" {cell:>11}");
                        medians.push(Some(r.median));
                    }
                    None => {
                        print!(" {:>10}", "--");
                        medians.push(None);
                    }
                }
            }
            if medians.len() >= 2 {
                if let (Some(oam), Some(baseline)) = (medians[0], medians[1]) {
                    let ratio = oam / baseline;
                    print!(" {:>7.2}x", ratio);
                } else {
                    print!(" {:>8}", "--");
                }
            }
            println!();
        }
    }
    println!();
}

fn print_ts_table(ts: &TsSection) {
    println!(
        "{:<20} {:>10} {:>10} {:>10} {:>6}",
        TS_CASE, "median", "min", "max", "unit"
    );
    println!("{}", "-".repeat(60));
    for row in &ts.rows {
        let r = &row.result;
        println!(
            "{:<20} {:>10.2} {:>10.2} {:>10.2} {:>6}",
            ts_row_label(&r.runtime, &row.variant),
            r.median,
            r.min,
            r.max,
            r.unit
        );
    }
    println!();
}

// -------------------------------------------------------------- JSON output

/// `exe` is the binary's file name only. The results file is committed to a
/// public repo, and the full path is the measuring box's layout (a home
/// directory, a per-run staging dir under temp) -- it identifies the box and
/// tells a reader nothing the `name` and `version` beside it do not.
fn runtime_json(rt: &Runtime) -> serde_json::Value {
    let exe = rt
        .exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| rt.name.clone());
    serde_json::json!({
        "name": rt.name,
        "version": rt.version,
        "exe": exe,
    })
}

/// One result row. `commit` is per row because a `--case` run can leave rows
/// from different trees in one file; on a full run every row's equals the
/// header's.
fn case_row_json(r: &CaseResult, commit: Option<&str>) -> serde_json::Value {
    let mut row = serde_json::json!({
        "runtime": r.runtime,
        "case": r.name,
        "unit": r.unit,
        "iterations": r.iterations,
        "warmup_iterations": r.warmup_iterations,
        "samples": r.samples,
        "p50": r.p50,
        "p95": r.p95,
        "p99": r.p99,
        "min": r.min,
        "max": r.max,
    });
    if let Some(commit) = commit {
        row["commit"] = serde_json::Value::String(commit.to_string());
    }
    row
}

fn build_json(
    results: &[CaseResult],
    runtimes: &[Runtime],
    ts: Option<&TsSection>,
    commit: &str,
    profile: &str,
) -> serde_json::Value {
    let rt_info: Vec<serde_json::Value> = runtimes.iter().map(runtime_json).collect();
    let cases: Vec<serde_json::Value> = results
        .iter()
        .map(|r| case_row_json(r, Some(commit)))
        .collect();

    let timestamp = chrono_utc_now();
    let host = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);

    let mut doc = serde_json::json!({
        "schema": "oam-bench/4",
        "timestamp": timestamp,
        "commit": commit,
        "host": host,
        "hardware_class": std::env::var("BENCH_HARDWARE_CLASS").unwrap_or_else(|_| "unknown".to_string()),
        "profile": profile,
        "warmup_iterations": 0,
        "runtimes": rt_info,
        "results": cases,
    });

    // The TypeScript section stamps itself: it is re-measured on its own by
    // `--case ts-cold-start`, and its rows are per cache state, not per
    // runtime, so they do not belong in `results`.
    if let Some(ts) = ts {
        let participants: Vec<serde_json::Value> = runtimes
            .iter()
            .filter(|rt| ts.rows.iter().any(|r| r.result.runtime == rt.name))
            .map(runtime_json)
            .collect();
        let rows: Vec<serde_json::Value> = ts
            .rows
            .iter()
            .map(|r| {
                let mut row = case_row_json(&r.result, None);
                row["variant"] = serde_json::Value::String(r.variant.clone());
                row
            })
            .collect();
        doc["ts_cold_start"] = serde_json::json!({
            "commit": commit,
            "profile": profile,
            "host": host,
            "timestamp": timestamp,
            "modules": TS_MODULES,
            "iterations": COLD_START_ITERS,
            "node_flag": ts.node_flag,
            "transpile_cache": TS_TRANSPILE_CACHE,
            "runtimes": participants,
            "rows": rows,
        });
    }
    doc
}

// ---------------------------------------------------------- markdown output

/// Render BENCHMARKS.md from the results document. Takes the JSON rather than
/// this run's structs so a `--case` merge and a full run render through one
/// path, and so the file can be regenerated from `bench/results.json` alone.
fn build_markdown(doc: &serde_json::Value) -> String {
    let text = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let num = |v: &serde_json::Value, k: &str| v[k].as_f64().unwrap_or(f64::NAN);
    let commit = text(doc, "commit");
    let profile = text(doc, "profile");
    let host = text(doc, "host");
    let runtimes: Vec<(String, String)> = doc["runtimes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|rt| (text(rt, "name"), text(rt, "version")))
                .collect()
        })
        .unwrap_or_default();
    let results: Vec<&serde_json::Value> = doc["results"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let find = |case: &str, runtime: &str| {
        results
            .iter()
            .copied()
            .find(|r| r["case"] == case && r["runtime"] == runtime)
    };

    let mut md = String::new();
    md.push_str("# Benchmarks\n\n");
    // The invocation has to be one a reader can paste. `cargo xtask bench`
    // needs a cargo alias configured; the portable spelling works from a bare
    // checkout, which is what someone reproducing these numbers has.
    md.push_str("Generated by `cargo run -p xtask -- bench` -- do not edit by hand.\n");
    md.push_str("Machine-readable twin: [`bench/results.json`](bench/results.json).\n\n");
    // Numbers without a method are a claim, not a measurement.
    md.push_str(concat!(
        "## Reproducing this\n\n",
        "These are from one machine and are not a leaderboard -- run them on yours:\n\n",
        "```sh\n",
        "cargo run -p xtask -- bench --release            # oam only\n",
        "cargo run -p xtask -- bench --release --compare  # also node and bun, on PATH\n",
        "```\n\n",
        "`--release` is load-bearing, and a prior `cargo build --release` does not imply ",
        "it: without the flag the harness builds and measures the DEBUG oam binary against ",
        "release `node.exe` and `bun.exe`, which is not a comparison. Every run stamps its ",
        "profile on the `Commit ... | <profile> | host ...` line below -- if that reads ",
        "`debug`, discard the numbers and re-run with `--release`.\n\n",
        "The oam binary is copied out of `target/` and exec'd once before timing starts. ",
        "A build directory is not a stable place to measure from: a concurrent `cargo ",
        "build` replaces the binary mid-run, and fresh bytes are cold where the installed ",
        "`node.exe` and `bun.exe` they are compared against are warm -- an asymmetry that ",
        "lands entirely on the one binary under test. Staging gives the run a private copy ",
        "nothing else is writing to. It is a file copy, needs no privileges, and changes no ",
        "result when nothing is rebuilding concurrently. If you hand-roll a ",
        "`target/release/oam` invocation while a build is running, that is why yours will ",
        "disagree.\n\n",
        "Each case is timed in-process by the harness rather than by a shell wrapper, ",
        "except `cold-start` and `mcp-cold-start`, which are wall-clock from process ",
        "spawn and can only be measured from outside. A runtime that is not on PATH is ",
        "skipped, never estimated. The profile and host are recorded in the line below, ",
        "because a debug build against release Node is not a comparison.\n\n",
        "Every figure is a MEDIAN over repeated process runs, and the per-case sample ",
        "count sits in the `iterations` field of [`bench/results.json`](bench/results.json) ",
        "alongside min/max/p95/p99 and the raw samples -- read those before drawing a ",
        "conclusion from a gap of a few percent. Runtimes are interleaved within a run, so ",
        "background load that drifts over the run lands on all of them rather than on ",
        "whichever was measured last; the ratios survive it, the absolute numbers do not. ",
        "Do not compare absolute figures across two runs on a machine that was doing ",
        "different things each time -- compare the ratios, or re-measure both on a quiet ",
        "box.\n\n",
        "Where another runtime wins, the table says so -- see ",
        "[docs/why-oam.md](docs/why-oam.md) for the workloads oam is and is not aimed ",
        "at.\n\n",
        "`--case <name>` (repeatable) measures only those cases and merges them into the ",
        "committed files instead of rewriting them: the `Commit` line below keeps describing ",
        "the last full run, every re-measured row is stamped with its own commit and listed ",
        "under the table, and the TypeScript section carries its own stamp. It is how one ",
        "case gets re-measured after a change without re-stamping numbers that did not ",
        "move.\n\n",
    ));
    md.push_str(&format!("Commit `{commit}` | {profile} | host {host}\n\n"));

    md.push_str("## Runtimes\n\n");
    for (name, version) in &runtimes {
        md.push_str(&format!("- **{name}** {version}\n"));
    }
    md.push('\n');

    md.push_str("## Results\n\n");
    // Not "all times in milliseconds": mcp-idle-rss is megabytes, and it sat
    // under that sentence unlabelled. Each row's own `unit` decides.
    md.push_str(
        "Milliseconds, except where a row's label names another unit. Lower is better.\n\n",
    );
    // `case` as the table prints it: the bare name for a millisecond row,
    // `name (UNIT)` for any other unit recorded on one of the case's rows.
    let case_label = |case: &str| {
        let unit = results
            .iter()
            .copied()
            .filter(|r| r["case"] == case)
            .find_map(|r| r["unit"].as_str())
            .unwrap_or("ms");
        if unit == "ms" {
            case.to_string()
        } else {
            format!("{case} ({unit})")
        }
    };

    if runtimes.len() == 1 {
        md.push_str("| Case | Median | Min | Max |\n");
        md.push_str("|---|--:|--:|--:|\n");
        for case_name in main_cases() {
            if let Some(r) = find(case_name, &runtimes[0].0) {
                md.push_str(&format!(
                    "| {} | {:.2} | {:.2} | {:.2} |\n",
                    case_label(case_name),
                    num(r, "p50"),
                    num(r, "min"),
                    num(r, "max")
                ));
            }
        }
    } else {
        md.push_str("| Case |");
        for (name, _) in &runtimes {
            md.push_str(&format!(" {name} |"));
        }
        md.push_str(&format!(" vs {} |", runtimes[1].0));
        md.push_str("\n|---|");
        for _ in &runtimes {
            md.push_str("--:|");
        }
        md.push_str("--:|\n");

        for case_name in main_cases() {
            md.push_str(&format!("| {} |", case_label(case_name)));
            let mut medians: Vec<Option<f64>> = Vec::new();
            for (name, _) in &runtimes {
                match find(case_name, name) {
                    Some(r) => {
                        let median = num(r, "p50");
                        md.push_str(&format!(" {median:.2} |"));
                        medians.push(Some(median));
                    }
                    None => {
                        md.push_str(" -- |");
                        medians.push(None);
                    }
                }
            }
            if let (Some(oam), Some(baseline)) = (medians[0], medians[1]) {
                let ratio = oam / baseline;
                md.push_str(&format!(" {ratio:.2}x |"));
            } else {
                md.push_str(" -- |");
            }
            md.push('\n');
        }
    }

    // Rows a `--case` run replaced describe a different tree than the header
    // line. Say which, or the stamp above silently covers numbers it did not
    // produce.
    let remeasured: Vec<String> = main_cases()
        .flat_map(|case| runtimes.iter().map(move |(name, _)| (case, name)))
        .filter_map(|(case, name)| {
            let row = find(case, name)?;
            let row_commit = row["commit"].as_str()?;
            let version = row["version"]
                .as_str()
                .map(|v| format!(" ({v})"))
                .unwrap_or_default();
            (row_commit != commit).then(|| format!("{case}/{name} at `{row_commit}`{version}"))
        })
        .collect();
    if !remeasured.is_empty() {
        md.push_str(&format!(
            "\nRe-measured by a filtered run (`--case`), so from a different tree than the \
             `Commit` line above: {}.\n",
            remeasured.join(", ")
        ));
    }
    md.push('\n');

    if let Some(ts) = doc.get("ts_cold_start").filter(|v| !v.is_null()) {
        push_ts_markdown(&mut md, ts);
    }

    md.push_str("## Cases\n\n");
    md.push_str("- **cold-start** -- wall-clock time from process spawn to exit (`console.log('ok')`). Measured by the harness, not JS.\n");
    md.push_str(
        "- **url-parse** -- 10,000 `new URL()` constructions across 5 representative URLs.\n",
    );
    md.push_str("- **http-throughput** -- node:http server + fetch client, 200 sequential requests on loopback.\n");
    md.push_str("- **http-keepalive-latency** -- milliseconds PER REQUEST of a small fetch on one warm pooled connection: node:http server on loopback, 50 warm-up fetches, then 10 batches of 200 sequential fetches of a 2-byte body; the median batch per run. The transport's per-request work, without connection setup (#183).\n");
    md.push_str("- **fs-read** -- `fs.readFileSync` of a 4KB file, 1,000 iterations.\n");
    md.push_str("- **json-parse** -- `JSON.parse(JSON.stringify(obj))` round-trip on a 100-user payload, 1,000 iterations.\n");
    md.push_str("- **crypto-hash** -- `crypto.createHash('sha256').update(64KB).digest()`, 1,000 iterations.\n");
    md.push_str("- **mcp-cold-start** -- wall-clock from process spawn to the first MCP `initialize` response via stdio. oam uses the built-in `oam:mcp` virtual module; other runtimes use `@modelcontextprotocol/sdk` (same noop tool, same transport).\n");
    md.push_str("- **mcp-idle-rss** -- resident set size (MB) after `initialize` completes and the server is idle, waiting for the next request. Measures runtime memory overhead of a hosted MCP server.\n");
    md.push_str("- **mcp-first-call-latency** -- wall-clock from sending `tools/call` to receiving the response, on an already-initialized server. Measures warm-path dispatch latency (no cold-start cost).\n");
    md.push_str(&format!(
        "- **{TS_CASE}** -- wall-clock from process spawn to the first stdout line of a generated \
         {TS_MODULES}-module TypeScript graph, one row per bytecode-cache state for oam with node \
         as the reference. Published in the TypeScript load path section above, which carries \
         its own stamp.\n"
    ));

    md
}

/// The `ts-cold-start` section: its own stamp line, table and legend.
fn push_ts_markdown(md: &mut String, ts: &serde_json::Value) {
    let text = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let num = |v: &serde_json::Value, k: &str| v[k].as_f64().unwrap_or(f64::NAN);
    let versions: Vec<String> = ts["runtimes"]
        .as_array()
        .map(|a| a.iter().map(|rt| text(rt, "version")).collect())
        .unwrap_or_default();
    let rows = ts["rows"].as_array().cloned().unwrap_or_default();
    let node_flag = ts["node_flag"].as_str();
    let has_node = rows.iter().any(|r| r["runtime"] == "node");

    md.push_str("## TypeScript load path\n\n");
    md.push_str(&format!(
        "Commit `{}` | {} | host {}",
        text(ts, "commit"),
        text(ts, "profile"),
        text(ts, "host")
    ));
    for version in &versions {
        md.push_str(&format!(" | {version}"));
    }
    md.push_str("\n\n");
    md.push_str(&format!(
        "`{TS_CASE}`: wall-clock from process spawn to the first stdout line of `main.ts`, the \
         entry of a generated graph of {} `.ts` modules (interfaces, generics, enums, `import \
         type`, a few inner functions each) that import each other in a chain plus a fan-out \
         from the entry. oam runs it as `oam run --no-check main.ts` in three cache states, one \
         row each; node runs it once, as installed, for reference. Median over {} runs per row, \
         the rows sampled round-robin like every other case. Milliseconds, lower is better. \
         Each label was checked rather than assumed: a no-cache run that leaves a `.v8c` blob, \
         a cold run that leaves none, or a row whose first line disagrees with another row's \
         fails the case instead of publishing. The stamp above is this section's own -- \
         `--case {TS_CASE}` re-measures it alone, without touching the table above.\n\n",
        ts["modules"], ts["iterations"]
    ));

    md.push_str("| Row | Median | Min | Max | p95 |\n|---|--:|--:|--:|--:|\n");
    for r in &rows {
        md.push_str(&format!(
            "| {} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
            ts_row_label(&text(r, "runtime"), &text(r, "variant")),
            num(r, "p50"),
            num(r, "min"),
            num(r, "max"),
            num(r, "p95")
        ));
    }
    md.push('\n');

    md.push_str(
        "- **oam no-cache** -- `OAM_CODE_CACHE=0`, fresh `OAM_CACHE_DIR` every run: the `.ts` -> JS \
         transpile (oxc) plus a full V8 compile; no bytecode is produced, written or read. The floor \
         without the cache.\n",
    );
    md.push_str(
        "- **oam cold** -- caches on, fresh `OAM_CACHE_DIR` every run: a project's first run, which \
         also serializes and writes the bytecode. Its gap above no-cache is the price of producing \
         the cache.\n",
    );
    // The committed BENCHMARKS.md carried this text (edited there when the
    // transpile cache shipped) while the generator still wrote the
    // pre-cache one, so a `--case` run reverted it.
    md.push_str(
        "- **oam warm** -- caches on, one `OAM_CACHE_DIR` primed by an untimed run and reused: every \
         run after the first. Its gap below no-cache is what consuming the cache saves. Both \
         caches are warm on this row: the V8 bytecode cache and, since the transpile cache \
         shipped, the oxc output for project files too (`OAM_TRANSPILE_CACHE`, on by default, \
         content-addressed by source text plus transpile settings).",
    );
    // The caveat is about ONE measurement, so it is keyed on that measurement
    // and not printed unconditionally: a section this harness measured records
    // the transpile-cache state it pinned (`ts_cmd`), and only a section
    // without that record is the older one the caveat describes. Printed
    // always, it told the reader to re-measure and then said the same thing
    // about the fresh measurement.
    if ts["transpile_cache"].is_null() {
        md.push_str(
            " The measurement above predates the transpile cache and so still describes a row \
             that re-ran oxc every time; re-measure with `--case ts-cold-start` before quoting \
             this row.",
        );
    }
    md.push('\n');
    match node_flag {
        Some(flag) if has_node => {
            md.push_str(&format!(
                "- **node** -- `node {flag} main.ts` as installed, `NODE_COMPILE_CACHE` unset."
            ));
            if flag == "--experimental-transform-types" {
                md.push_str(
                    " Strip-only mode (`--experimental-strip-types`) rejects the fixture's enums with \
                     `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`, so the reference is transform mode.",
                );
            }
            md.push('\n');
        }
        _ => md.push_str(
            "- **node** -- no row: node was not on PATH, `--compare` was not passed, or neither \
             `--experimental-strip-types` nor `--experimental-transform-types` ran the fixture; the \
             run's console output says which.\n",
        ),
    }
    md.push('\n');
}

// -------------------------------------------------------- shared helpers

// `repo_root`, `ensure_oam_built`, `capture_version`, `git_short_commit` and
// `run_with_timeout` (with its `Captured`) live in `conformance` and are
// imported at the top of this file rather than
// re-declared here. They used to be copy-pasted into both modules, and the
// copies DIVERGED: on 2026-08-04 conformance's `ensure_oam_built` was fixed to
// always invoke cargo (an existing-but-stale binary was silently measured for
// weeks), and bench's copy kept the `if !exe.is_file()` guard the fix removed.
// A bench run therefore measured whatever binary happened to be sitting in
// target/ and stamped it with today's HEAD -- a plausible number attributed to
// code it did not come from, which is worse than a loud failure. One
// definition means the next fix cannot land in only half the callers.

/// ISO 8601 UTC timestamp of now, from the system clock.
///
/// Computed here rather than by spawning `date`: Windows has no `date.exe`
/// (it is a cmd builtin), so outside a shell with Git's tools on PATH the
/// spawn failed and the results file got an empty timestamp, silently.
fn chrono_utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    utc_timestamp(secs)
}

/// `secs` since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ` (proleptic Gregorian,
/// no leap seconds -- the same reading `date -u` gives).
fn utc_timestamp(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Days since 1970-01-01 to a civil date, counting in 400-year eras that
    // start on 0000-03-01 so the leap day is the last day of a year.
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153; // 0 = March
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod timestamp_tests {
    use super::utc_timestamp;

    #[test]
    fn epoch_seconds_render_as_iso_8601_utc() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(86_399), "1970-01-01T23:59:59Z");
        // A leap day in a century year that IS a leap year, and the day after.
        assert_eq!(utc_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_timestamp(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(utc_timestamp(1_700_000_000), "2023-11-14T22:13:20Z");
        // Year boundary.
        assert_eq!(utc_timestamp(1_704_067_199), "2023-12-31T23:59:59Z");
        assert_eq!(utc_timestamp(1_704_067_200), "2024-01-01T00:00:00Z");
    }
}

/// Every document that quotes the benchmark table must name the run that
/// produced `bench/results.json`.
///
/// `BENCHMARKS.md` is generated from that file, but `docs/why-oam.md` and
/// `docs/blog/why-oam.md` re-state the same numbers in hand-written prose, and
/// nothing forced the three to agree. On 2026-08-10 they carried numbers from
/// three DIFFERENT runs simultaneously: the blog said "the committed table in
/// `BENCHMARKS.md` is generated ... at commit `b078584`" and then printed a
/// table that was not the one in `BENCHMARKS.md` -- which was `f096b5d`, a
/// different oam version, and measured under load (its cold-start was ~2x both
/// neighbouring runs, against a ~10% within-run spread). A reader who checked
/// the cited source found it contradicted the citation.
///
/// Comparing against `results.json` rather than against git HEAD is deliberate.
/// The invariant is "every doc describes the run it quotes", which stays true
/// across later commits that do not re-run the bench; gating on HEAD would go
/// red on the next unrelated commit and train people to ignore it.
#[cfg(test)]
mod bench_doc_stamp {
    use serde_json::Value;

    const RESULTS: &str = include_str!("../../bench/results.json");

    /// Every doc that quotes the table, with its contents baked in so the test
    /// needs no filesystem and no repo-root discovery.
    const DOCS: &[(&str, &str)] = &[
        ("BENCHMARKS.md", include_str!("../../BENCHMARKS.md")),
        ("docs/why-oam.md", include_str!("../../docs/why-oam.md")),
        (
            "docs/blog/why-oam.md",
            include_str!("../../docs/blog/why-oam.md"),
        ),
    ];

    fn results() -> Value {
        serde_json::from_str(RESULTS).expect("bench/results.json parses as JSON")
    }

    #[test]
    fn every_bench_doc_names_the_run_it_quotes() {
        let results = results();
        let commit = results["commit"]
            .as_str()
            .expect("bench/results.json carries a `commit` field");
        assert!(
            !commit.is_empty() && commit != "unknown",
            "bench/results.json records commit `{commit}`, which names no run. \
             Regenerate: cargo run -p xtask -- bench --release --compare",
        );

        let needle = format!("`{commit}`");
        let stale: Vec<&str> = DOCS
            .iter()
            .filter(|(_, body)| !body.contains(&needle))
            .map(|(path, _)| *path)
            .collect();

        assert!(
            stale.is_empty(),
            "bench/results.json is from commit `{commit}`, but these docs do not \
             cite it: {stale:?}.\nThey quote the benchmark table, so each one has to \
             say which run the numbers came from -- otherwise they drift apart \
             silently, which is exactly what happened on 2026-08-10.\nFix by \
             re-running `cargo run -p xtask -- bench --release --compare` (that \
             rewrites BENCHMARKS.md) and hand-updating the prose docs to match.",
        );
    }

    #[test]
    fn quoted_numbers_come_from_a_release_build() {
        let profile = results()["profile"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            profile, "release",
            "bench/results.json was measured on a `{profile}` build. A debug oam \
             against release node.exe and bun.exe is not a comparison, and these \
             numbers are published. Re-run with --release.",
        );
    }
}

#[cfg(all(test, target_os = "windows"))]
mod rss_parse_tests {
    // The production parse itself (`process_rss_bytes` calls it), not a copy:
    // a regression there fails these tests.
    use super::parse_tasklist_rss_kb;

    #[test]
    fn thousands_separator_does_not_scramble_the_value() {
        // Verbatim `tasklist /FO CSV /NH` output. The memory field carries a
        // thousands separator, which the previous splitn-on-',' parse cut in
        // half and rejoined backwards: 22,220 K came out as 22022.
        let real = r#""System","4","Services","0","22,220 K""#;
        assert_eq!(parse_tasklist_rss_kb(real), Some(22220));

        // Bigger values are where the old parse went furthest wrong: this one
        // reported 79284 (77.4 MB) for a process actually holding 82.8 MB.
        let big = r#""oam.exe","31448","Console","1","84,792 K""#;
        assert_eq!(parse_tasklist_rss_kb(big), Some(84792));

        // Millions carry two separators -- the old parse kept only two of the
        // three fragments and dropped the leading digits entirely.
        let huge = r#""chrome.exe","9012","Console","1","1,234,567 K""#;
        assert_eq!(parse_tasklist_rss_kb(huge), Some(1234567));

        // Under 1000 K there is no separator at all, which is the ONLY shape
        // the old parse got right -- hence it looked like it worked.
        let small = r#""tiny.exe","7","Console","1","840 K""#;
        assert_eq!(parse_tasklist_rss_kb(small), Some(840));
    }

    #[test]
    fn runtime_json_records_the_file_name_not_the_local_path() {
        let dir = std::env::temp_dir().join("oam-bench-stage-0");
        let rt = super::Runtime {
            name: "oam".to_string(),
            exe: dir.join("oam-bin"),
            version: "oam 0.0.0".to_string(),
        };
        let json = super::runtime_json(&rt);
        assert_eq!(json["exe"], "oam-bin");
        assert_eq!(json["name"], "oam");
        assert_eq!(json["version"], "oam 0.0.0");
    }

    /// The committed results file must not carry a directory in any `exe`.
    #[test]
    fn committed_results_carry_no_local_paths() {
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../../bench/results.json")).expect("results parse");
        for list in [&doc["runtimes"], &doc["ts_cold_start"]["runtimes"]] {
            for rt in list.as_array().into_iter().flatten() {
                let exe = rt["exe"].as_str().unwrap_or("");
                assert!(
                    !exe.contains('/') && !exe.contains('\\'),
                    "bench/results.json records a path in `exe`: {exe}"
                );
            }
        }
    }

    #[test]
    fn a_dead_pid_yields_none_rather_than_a_bogus_number() {
        // tasklist prints this to stdout when the filter matches nothing. It
        // must not parse as a memory figure.
        let gone = "INFO: No tasks are running which match the specified criteria.";
        assert_eq!(parse_tasklist_rss_kb(gone), None);
        assert_eq!(parse_tasklist_rss_kb(""), None);
    }
}

/// `process_rss_bytes` against a live PID (this test process), so each OS leg
/// runs its own arm end to end: tasklist on Windows, /proc VmRSS on Linux,
/// sysinfo on macOS. Plus the Linux arm's pure parse, which compiles under
/// `test` on every host.
#[cfg(test)]
mod rss_live_tests {
    use super::{parse_proc_status_vmrss_kb, process_rss_bytes};

    #[test]
    fn own_pid_reports_a_byte_count_in_a_sane_range() {
        let bytes = process_rss_bytes(std::process::id())
            .expect("RSS of the running test process must be readable");
        // A Rust test binary with std, serde_json and the test harness loaded
        // holds several MiB resident on every OS. The tasklist and VmRSS arms
        // both report kB; a dropped `* 1024` would land this in the kB range
        // (a few thousand to tens of thousands), far under 2 MiB.
        const MIB: u64 = 1024 * 1024;
        assert!(
            bytes >= 2 * MIB,
            "RSS {bytes} bytes is implausibly small for a live process -- kB reported as bytes?"
        );
        // Ceiling: this process holds roughly 7-50 MiB resident, so a `* 1024`
        // applied twice (or to a figure already in bytes) reads as at least
        // 1 GiB -- measured: the double-scaled mutant reported ~7.4 GiB.
        assert!(
            bytes < 1024 * MIB,
            "RSS {bytes} bytes is implausibly large for a test process -- unit scaled twice?"
        );
    }

    #[test]
    fn vmrss_parse_reads_the_kb_field() {
        // Shape of a real /proc/<pid>/status excerpt: VmRSS is not the first
        // Vm* line, and VmHWM (peak) precedes it with a larger value.
        let status = "Name:\tcargo\nUmask:\t0022\nState:\tS (sleeping)\n\
                      VmPeak:\t  123456 kB\nVmSize:\t  120000 kB\n\
                      VmHWM:\t   30000 kB\nVmRSS:\t   12345 kB\nRssAnon:\t    8000 kB\n";
        assert_eq!(parse_proc_status_vmrss_kb(status), Some(12345));
    }

    #[test]
    fn vmrss_parse_without_the_line_yields_none() {
        // Kernel threads (and zombies) carry no VmRSS line at all.
        let kthread = "Name:\tkworker/0:1\nState:\tI (idle)\nThreads:\t1\n";
        assert_eq!(parse_proc_status_vmrss_kb(kthread), None);
        assert_eq!(parse_proc_status_vmrss_kb(""), None);
        // A malformed value must not turn into a number.
        assert_eq!(parse_proc_status_vmrss_kb("VmRSS:\n"), None);
        assert_eq!(parse_proc_status_vmrss_kb("VmRSS:\t abc kB\n"), None);
    }
}

/// The `--case` path and the ts-cold-start fixture, exercised without
/// spawning a runtime. What these protect: a filtered run must never move the
/// header stamp (the doc-stamp test above depends on it), must never keep a
/// stale number under a fresh label, and the fixture must be the graph the
/// section describes.
#[cfg(test)]
mod filtered_runs {
    use super::*;
    use serde_json::{Value, json};

    fn row(case: &str, runtime: &str, p50: f64, commit: Option<&str>) -> Value {
        let mut row = json!({
            "case": case, "runtime": runtime, "unit": "ms", "iterations": 1,
            "warmup_iterations": 0, "samples": [p50], "p50": p50, "p95": p50,
            "p99": p50, "min": p50, "max": p50,
        });
        if let Some(commit) = commit {
            row["commit"] = json!(commit);
        }
        row
    }

    fn existing() -> Value {
        json!({
            "schema": "oam-bench/3", "commit": "acec008", "profile": "release",
            "host": "test-host", "timestamp": "t0",
            "runtimes": [
                {"name": "oam", "version": "oam 0.9.0", "exe": "oam"},
                {"name": "node", "version": "v22", "exe": "node"},
            ],
            "results": [
                row("cold-start", "oam", 59.0, None),
                row("cold-start", "node", 113.0, None),
                row("url-parse", "oam", 6.5, None),
            ],
            "ts_cold_start": {"commit": "acec008", "rows": []},
        })
    }

    fn fresh(results: Vec<Value>, ts: Option<Value>) -> Value {
        let mut doc = json!({
            "schema": "oam-bench/4", "commit": "01bee57+wip", "profile": "release",
            "host": "test-host", "timestamp": "t1",
            "runtimes": [{"name": "oam", "version": "oam 0.13.0", "exe": "oam"}],
            "results": results,
        });
        if let Some(ts) = ts {
            doc["ts_cold_start"] = ts;
        }
        doc
    }

    #[test]
    fn case_filter_rejects_unknown_names_and_keeps_table_order() {
        assert!(select_cases(&["nope".to_string()]).is_err());
        let picked = select_cases(&["mcp-idle-rss".to_string(), "cold-start".to_string()]).unwrap();
        assert_eq!(picked, vec!["cold-start", "mcp-idle-rss"]);
        assert_eq!(select_cases(&[]).unwrap(), CASE_NAMES.to_vec());
    }

    #[test]
    fn merge_replaces_only_what_was_measured_and_keeps_the_header_stamp() {
        let fresh = fresh(
            vec![row("cold-start", "oam", 40.0, Some("01bee57+wip"))],
            Some(json!({"commit": "01bee57+wip", "rows": [{"runtime": "oam"}]})),
        );
        let merged =
            merge_filtered_run(existing(), fresh, &["cold-start", TS_CASE], &["oam"]).unwrap();

        // The header keeps describing the last full run: the doc-stamp test
        // and the prose docs both key off it.
        assert_eq!(merged["commit"], "acec008");
        assert_eq!(merged["schema"], "oam-bench/4");

        let rows = merged["results"].as_array().unwrap();
        let find = |case: &str, rt: &str| {
            rows.iter()
                .find(|r| r["case"] == case && r["runtime"] == rt)
                .unwrap_or_else(|| panic!("{case}/{rt} missing"))
        };
        assert_eq!(find("cold-start", "oam")["p50"], 40.0);
        assert_eq!(find("cold-start", "oam")["commit"], "01bee57+wip");
        assert_eq!(find("cold-start", "oam")["version"], "oam 0.13.0");
        // node was not measured (no --compare), so its row and its lack of a
        // per-row stamp survive untouched.
        assert_eq!(find("cold-start", "node")["p50"], 113.0);
        assert!(find("cold-start", "node")["commit"].is_null());
        assert_eq!(find("url-parse", "oam")["p50"], 6.5);
        assert_eq!(rows.len(), 3);

        // Runtimes: kept as the full run had them -- url-parse/oam is still
        // that run's oam 0.9.0 -- so the columns do not shift and no old row
        // is relabelled; the fresh row says its own version.
        let runtimes = merged["runtimes"].as_array().unwrap();
        assert_eq!(runtimes[0]["version"], "oam 0.9.0");
        assert_eq!(runtimes[1]["name"], "node");
        assert!(find("url-parse", "oam")["version"].is_null());

        assert_eq!(merged["ts_cold_start"]["commit"], "01bee57+wip");
    }

    #[test]
    fn merge_adds_a_runtime_the_full_run_lacked_and_relabels_none() {
        let mut fresh = fresh(
            vec![
                row("cold-start", "oam", 40.0, Some("01bee57")),
                row("cold-start", "bun", 20.0, Some("01bee57")),
            ],
            None,
        );
        fresh["runtimes"] = json!([
            {"name": "oam", "version": "oam 0.13.0", "exe": "oam"},
            {"name": "bun", "version": "1.2", "exe": "bun"},
        ]);
        let merged =
            merge_filtered_run(existing(), fresh, &["cold-start"], &["oam", "bun"]).unwrap();
        let names: Vec<&str> = merged["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|rt| rt["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["oam", "node", "bun"]);
        assert_eq!(merged["runtimes"][0]["version"], "oam 0.9.0");
        let md = build_markdown(&merged);
        assert!(
            md.contains("cold-start/oam at `01bee57` (oam 0.13.0)"),
            "{md}"
        );
        assert!(md.contains("cold-start/bun at `01bee57` (1.2)"), "{md}");
    }

    #[test]
    fn merge_drops_a_measured_row_that_failed_rather_than_keeping_the_old_number() {
        let fresh = fresh(vec![], None);
        let merged = merge_filtered_run(existing(), fresh, &["cold-start"], &["oam"]).unwrap();
        let rows = merged["results"].as_array().unwrap();
        assert!(
            !rows
                .iter()
                .any(|r| r["case"] == "cold-start" && r["runtime"] == "oam"),
            "a failed measurement must show as missing, not as last run's number"
        );
        assert!(
            rows.iter()
                .any(|r| r["case"] == "cold-start" && r["runtime"] == "node")
        );
        // The TypeScript section was not selected, so it is kept as it was.
        assert_eq!(merged["ts_cold_start"]["commit"], "acec008");
        // No fresh main-table row landed, so the runtimes list -- which
        // describes the binaries behind the main table -- stays as it was.
        assert_eq!(merged["runtimes"][0]["version"], "oam 0.9.0");
    }

    #[test]
    fn merge_keeps_the_ts_section_when_its_measurement_failed() {
        let fresh = fresh(vec![], None);
        let merged = merge_filtered_run(existing(), fresh, &[TS_CASE], &["oam"]).unwrap();
        assert_eq!(merged["ts_cold_start"]["commit"], "acec008");
    }

    #[test]
    fn merge_refuses_to_mix_profiles_or_hosts() {
        let mut debug = fresh(vec![], None);
        debug["profile"] = json!("debug");
        assert!(merge_filtered_run(existing(), debug, &["cold-start"], &["oam"]).is_err());
        let mut other_box = fresh(vec![], None);
        other_box["host"] = json!("macos-aarch64");
        assert!(merge_filtered_run(existing(), other_box, &["cold-start"], &["oam"]).is_err());
    }

    #[test]
    fn markdown_keeps_the_header_stamp_and_flags_remeasured_rows() {
        let fresh = fresh(
            vec![row("cold-start", "oam", 40.0, Some("01bee57+wip"))],
            Some(json!({
                "commit": "01bee57+wip", "profile": "release", "host": "test-host",
                "modules": TS_MODULES, "iterations": 20,
                "node_flag": "--experimental-transform-types",
                "runtimes": [
                    {"name": "oam", "version": "oam 0.13.0"},
                    {"name": "node", "version": "v22"},
                ],
                "rows": [
                    row(TS_CASE, "oam", 70.0, None).tap(|r| r["variant"] = json!("no-cache")),
                    row(TS_CASE, "oam", 60.0, None).tap(|r| r["variant"] = json!("warm")),
                    row(TS_CASE, "node", 90.0, None).tap(|r| r["variant"] = json!("reference")),
                ],
            })),
        );
        let merged =
            merge_filtered_run(existing(), fresh, &["cold-start", TS_CASE], &["oam"]).unwrap();
        let md = build_markdown(&merged);

        assert!(md.contains("Commit `acec008` | release | host test-host"));
        assert!(md.contains("| cold-start | 40.00 | 113.00 | 0.35x |"));
        assert!(md.contains("cold-start/oam at `01bee57+wip` (oam 0.13.0)"));
        assert!(md.contains("- **oam** oam 0.9.0"), "{md}");
        assert!(md.contains("## TypeScript load path"));
        assert!(md.contains("Commit `01bee57+wip` | release | host test-host | oam 0.13.0 | v22"));
        assert!(md.contains("| oam no-cache | 70.00 |"));
        assert!(md.contains("| oam warm | 60.00 |"));
        assert!(md.contains("| node | 90.00 |"));
        assert!(md.contains("`node --experimental-transform-types main.ts`"));
        assert!(md.contains("rejects the fixture's enums"));
    }

    #[test]
    fn markdown_without_a_remeasured_row_has_no_footnote() {
        let md = build_markdown(&existing());
        assert!(!md.contains("Re-measured by a filtered run"));
        assert!(md.contains("| cold-start | 59.00 | 113.00 | 0.52x |"));
    }

    /// mcp-idle-rss is megabytes; the table must say so in the row rather
    /// than sit under a blanket "milliseconds".
    #[test]
    fn markdown_labels_a_non_millisecond_row_with_its_unit() {
        let mut doc = existing();
        let mut rss = row("mcp-idle-rss", "oam", 25.0, None);
        rss["unit"] = json!("MB");
        doc["results"].as_array_mut().unwrap().push(rss);
        let md = build_markdown(&doc);
        assert!(md.contains("| mcp-idle-rss (MB) | 25.00 |"), "{md}");
        assert!(md.contains("| cold-start | 59.00 |"), "{md}");
        assert!(!md.contains("All times in milliseconds"), "{md}");
    }

    /// The "predates the transpile cache" caveat describes one measurement.
    /// A section that records the pinned transpile-cache state is not it.
    #[test]
    fn ts_legend_caveat_follows_the_measurement_not_the_generator() {
        let section = |pinned: bool| {
            let mut ts = json!({
                "commit": "c", "profile": "release", "host": "h",
                "modules": TS_MODULES, "iterations": 20, "node_flag": null,
                "runtimes": [], "rows": [],
            });
            if pinned {
                ts["transpile_cache"] = json!(TS_TRANSPILE_CACHE);
            }
            let mut md = String::new();
            push_ts_markdown(&mut md, &ts);
            md
        };
        assert!(section(false).contains("predates the transpile cache"));
        assert!(!section(true).contains("predates the transpile cache"));
        assert!(section(true).contains("Both caches are warm on this row"));
    }

    /// A row label promises a cache state, so no cache variable may be left
    /// to the caller's environment.
    #[test]
    fn ts_rows_pin_every_cache_variable() {
        let rt = Runtime {
            name: "oam".to_string(),
            exe: PathBuf::from("oam"),
            version: String::new(),
        };
        for state in TsState::OAM {
            let cmd = ts_cmd(&rt, state, Path::new("main.ts"), Path::new("cache"), None);
            let set: Vec<String> = cmd
                .get_envs()
                .filter(|(_, value)| value.is_some())
                .map(|(key, _)| key.to_string_lossy().into_owned())
                .collect();
            for key in ["OAM_CACHE_DIR", "OAM_CODE_CACHE", "OAM_TRANSPILE_CACHE"] {
                assert!(
                    set.iter().any(|k| k == key),
                    "the {} row inherits {key}",
                    state.variant()
                );
            }
        }
    }

    #[test]
    fn results_header_has_a_timestamp_and_no_ci_run_field() {
        let doc = build_json(&[], &[], None, "abc1234", "release");
        assert!(doc["ci_run_id"].is_null());
        let timestamp = doc["timestamp"].as_str().unwrap_or_default();
        assert_eq!(timestamp.len(), "1970-01-01T00:00:00Z".len(), "{timestamp}");
        assert!(timestamp.ends_with('Z'), "{timestamp}");
    }

    /// An npm-installed tool leaves a bare sh shim beside `tool.cmd`; on
    /// Windows the lookup must return the one `Command` can exec.
    #[test]
    fn which_prefers_an_executable_suffix_over_a_bare_name_on_windows() {
        let dir = std::env::temp_dir().join(format!("oam-bench-which-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("tool.cmd"), "@echo off\n").unwrap();
        let dirs = [dir.clone()];
        assert_eq!(which_in("tool", &dirs, true), Some(dir.join("tool.cmd")));
        assert_eq!(which_in("tool", &dirs, false), Some(dir.join("tool")));
        std::fs::write(dir.join("tool.exe"), "").unwrap();
        assert_eq!(which_in("tool", &dirs, true), Some(dir.join("tool.exe")));
        assert_eq!(which_in("absent", &dirs, true), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    trait Tap: Sized {
        fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
            f(&mut self);
            self
        }
    }
    impl Tap for Value {}

    #[test]
    fn ts_fixture_is_deterministic_and_shaped_as_documented() {
        let base =
            std::env::temp_dir().join(format!("oam-bench-ts-fixture-{}", std::process::id()));
        let a = write_ts_fixture(&base.join("a")).unwrap();
        let b = write_ts_fixture(&base.join("b")).unwrap();
        let read = |dir: &Path, name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
        let dir_a = a.entry.parent().unwrap();
        let dir_b = b.entry.parent().unwrap();

        // Byte-identical across generations: the bytecode cache keys on
        // source text, so a wobble here would silently turn warm into cold.
        for i in 1..=TS_MODULES {
            let name = format!("mod{i:02}.ts");
            assert_eq!(read(dir_a, &name), read(dir_b, &name), "{name} differs");
        }
        assert_eq!(read(dir_a, "main.ts"), read(dir_b, "main.ts"));
        assert_eq!(
            std::fs::read_dir(dir_a).unwrap().count(),
            TS_MODULES + 2,
            "modules + main.ts + package.json"
        );

        // The chain: each module imports the next by value and by type; the
        // last imports nothing.
        let first = read(dir_a, "mod01.ts");
        assert!(first.contains("import { make02, run02 } from './mod02.ts';"));
        assert!(first.contains("import type { Record02 } from './mod02.ts';"));
        assert!(first.contains("export enum Stage01"));
        assert!(first.contains("child?: Record02;"));
        let last = read(dir_a, &format!("mod{TS_MODULES:02}.ts"));
        assert!(!last.contains("import "));
        assert!(!last.contains("child"));

        // The fan-out: the entry imports module 1 and every stride-th one.
        let entry = read(dir_a, "main.ts");
        assert!(entry.contains("import type { Record01 } from './mod01.ts';"));
        for i in ts_fanout() {
            assert!(
                entry.contains(&format!("from './mod{i:02}.ts';")),
                "entry misses mod{i:02}"
            );
            assert!(entry.contains(&format!("run{i:02}({i})")));
        }
        assert!(entry.contains("from './mod01.ts';"));
        assert!(!entry.contains("from './mod02.ts';"));
        assert!(read(dir_a, "package.json").contains("\"type\":\"module\""));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn row_labels_and_error_lines() {
        assert_eq!(ts_row_label("oam", "no-cache"), "oam no-cache");
        assert_eq!(ts_row_label("node", "reference"), "node");
        // Node prints the offending source and a caret before the error line;
        // the error line is the one worth quoting.
        let node_stderr = "C:\\x\\main.ts:2\nenum Color { Red = 1 }\n^^^^\n\nSyntaxError [ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX]: TypeScript enum is not supported in strip-only mode\n    at parseTypeScript";
        assert!(
            error_line(node_stderr).starts_with("SyntaxError [ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX]")
        );
        assert_eq!(error_line("\n  plain failure\n"), "plain failure");
        assert_eq!(error_line(""), "");
    }
}
