//! `cargo xtask bench`: micro-benchmark harness for the oam JS runtime.
//!
//! Four benchmark cases measure different layers of the stack:
//!   1. cold-start  — V8 isolate creation + bootstrap + module load + exit
//!   2. url-parse   — URL constructor throughput (10 000 parses)
//!   3. http-throughput — HTTP server request handling (req/s)
//!   4. fs-read     — fs.readFileSync hot-loop (1 000 reads)
//!
//! Each case writes a temp JS file, runs `oam run <file> --no-check`, and
//! parses a single JSON line from stdout for timing data.  The harness
//! reports both a human-readable table and a machine-readable JSON summary.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ config

const COLD_START_ITERS: usize = 10;
const URL_PARSE_ITERS: usize = 5;
const HTTP_THROUGHPUT_ITERS: usize = 3;
const FS_READ_ITERS: usize = 5;

// Generous per-case timeout — benchmarks should finish quickly but we
// don't want spurious kills on a loaded machine.
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

// ------------------------------------------------------------------ entry

pub fn run(release: bool) -> Result<()> {
    let repo = repo_root()?;
    let oam = ensure_oam_built(&repo, release)?;
    let profile = if release { "release" } else { "debug" };

    let oam_version = capture_version(&oam, &["--version"]);
    let commit = git_short_commit(&repo);
    println!("oam {oam_version} ({profile}) @ {commit}\n");

    let tmp = std::env::temp_dir().join(format!("oam-bench-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;

    let results: Vec<CaseResult> = vec![
        bench_cold_start(&oam, &tmp)?,
        bench_url_parse(&oam, &tmp)?,
        bench_http_throughput(&oam, &tmp)?,
        bench_fs_read(&oam, &tmp)?,
    ];

    // Clean up temp dir (best-effort).
    let _ = std::fs::remove_dir_all(&tmp);

    // ------------------------------------------------------------- table
    println!();
    println!(
        "{:<20} {:>8} {:>8} {:>8} {:>6} {:>6}",
        "case", "median", "min", "max", "unit", "iters"
    );
    println!("{}", "-".repeat(62));
    for r in &results {
        println!(
            "{:<20} {:>8.2} {:>8.2} {:>8.2} {:>6} {:>6}",
            r.name, r.median, r.min, r.max, r.unit, r.iterations
        );
    }
    println!();

    // -------------------------------------------------------------- JSON
    let json = build_json(&results, &oam_version, &commit, profile);
    println!("{json}");

    Ok(())
}

// ------------------------------------------------------------ case runner

struct CaseResult {
    name: String,
    median: f64,
    min: f64,
    max: f64,
    unit: String,
    iterations: usize,
    samples: Vec<f64>,
}

fn run_case(
    name: &str,
    oam: &Path,
    script_path: &Path,
    iters: usize,
    extract: fn(&str) -> Result<f64>,
) -> Result<CaseResult> {
    print!("{name}: ");
    let mut samples = Vec::with_capacity(iters);
    for i in 0..iters {
        let out = run_with_timeout(
            Command::new(oam)
                .arg("run")
                .arg(script_path)
                .arg("--no-check"),
            CASE_TIMEOUT,
        )?;
        if out.timed_out {
            bail!("{name}: iteration {i} timed out");
        }
        if out.code != 0 {
            bail!(
                "{name}: iteration {i} exited with code {}; stderr: {}",
                out.code,
                out.stderr
            );
        }
        let value = extract(&out.stdout)
            .with_context(|| format!("{name}: iteration {i} output parse failed"))?;
        samples.push(value);
        print!(".");
    }
    println!(" done");

    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];

    Ok(CaseResult {
        name: name.to_string(),
        median,
        min,
        max,
        unit: "ms".to_string(),
        iterations: iters,
        samples,
    })
}

// ------------------------------------------------------------- cold-start
// Measures wall-clock time from process start to exit on a trivial script.
// Timing is done by the harness (Rust side), not by JS.

fn bench_cold_start(oam: &Path, tmp: &Path) -> Result<CaseResult> {
    let script = tmp.join("bench_cold_start.mjs");
    std::fs::write(&script, "console.log('ok');\n")?;

    print!("cold-start: ");
    let mut samples = Vec::with_capacity(COLD_START_ITERS);
    for i in 0..COLD_START_ITERS {
        let start = Instant::now();
        let out = run_with_timeout(
            Command::new(oam).arg("run").arg(&script).arg("--no-check"),
            CASE_TIMEOUT,
        )?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        if out.timed_out {
            bail!("cold-start: iteration {i} timed out");
        }
        if out.code != 0 {
            bail!(
                "cold-start: iteration {i} exited with code {}; stderr: {}",
                out.code,
                out.stderr
            );
        }
        samples.push(elapsed_ms);
        print!(".");
    }
    println!(" done");

    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];

    Ok(CaseResult {
        name: "cold-start".to_string(),
        median,
        min,
        max,
        unit: "ms".to_string(),
        iterations: COLD_START_ITERS,
        samples,
    })
}

// -------------------------------------------------------------- url-parse

fn bench_url_parse(oam: &Path, tmp: &Path) -> Result<CaseResult> {
    let script = tmp.join("bench_url_parse.mjs");
    std::fs::write(
        &script,
        r#"
const N = 10000;
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

    run_case("url-parse", oam, &script, URL_PARSE_ITERS, |stdout| {
        let v: serde_json::Value = serde_json::from_str(stdout.trim())?;
        v["elapsed_ms"]
            .as_f64()
            .context("missing elapsed_ms in output")
    })
}

// -------------------------------------------------------- http-throughput

fn bench_http_throughput(oam: &Path, tmp: &Path) -> Result<CaseResult> {
    let script = tmp.join("bench_http_throughput.mjs");
    std::fs::write(
        &script,
        r#"
import http from 'node:http';
const N = 200;
const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end('ok');
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;
const base = `http://127.0.0.1:${port}`;
const t0 = performance.now();
for (let i = 0; i < N; i++) {
    const res = await fetch(`${base}/bench`);
    await res.text();
}
const elapsed = performance.now() - t0;
server.close();
const rps = (N / elapsed) * 1000;
console.log(JSON.stringify({ elapsed_ms: elapsed, requests: N, rps: rps }));
"#,
    )?;

    let mut result = run_case(
        "http-throughput",
        oam,
        &script,
        HTTP_THROUGHPUT_ITERS,
        |stdout| {
            let v: serde_json::Value = serde_json::from_str(stdout.trim())?;
            v["elapsed_ms"]
                .as_f64()
                .context("missing elapsed_ms in output")
        },
    )?;
    result.unit = "ms".to_string();
    Ok(result)
}

// ---------------------------------------------------------------- fs-read

fn bench_fs_read(oam: &Path, tmp: &Path) -> Result<CaseResult> {
    // Write a known file for the benchmark to read.
    let data_file = tmp.join("bench_data.txt");
    std::fs::write(&data_file, "x".repeat(4096))?;

    let script = tmp.join("bench_fs_read.mjs");
    // The script receives the data file path via argv.
    std::fs::write(
        &script,
        r#"
import fs from 'node:fs';
const filePath = process.argv[2];
const N = 1000;
const t0 = performance.now();
for (let i = 0; i < N; i++) {
    fs.readFileSync(filePath, 'utf8');
}
const elapsed = performance.now() - t0;
console.log(JSON.stringify({ elapsed_ms: elapsed, reads: N }));
"#,
    )?;

    // For fs-read we need to pass the data file path as an extra arg.
    print!("fs-read: ");
    let mut samples = Vec::with_capacity(FS_READ_ITERS);
    for i in 0..FS_READ_ITERS {
        let out = run_with_timeout(
            Command::new(oam)
                .arg("run")
                .arg(&script)
                .arg("--no-check")
                .arg("--")
                .arg(&data_file),
            CASE_TIMEOUT,
        )?;
        if out.timed_out {
            bail!("fs-read: iteration {i} timed out");
        }
        if out.code != 0 {
            bail!(
                "fs-read: iteration {i} exited with code {}; stderr: {}",
                out.code,
                out.stderr
            );
        }
        let v: serde_json::Value = serde_json::from_str(out.stdout.trim())
            .with_context(|| format!("fs-read: iteration {i} output parse failed"))?;
        let value = v["elapsed_ms"]
            .as_f64()
            .context("missing elapsed_ms in fs-read output")?;
        samples.push(value);
        print!(".");
    }
    println!(" done");

    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];

    Ok(CaseResult {
        name: "fs-read".to_string(),
        median,
        min,
        max,
        unit: "ms".to_string(),
        iterations: FS_READ_ITERS,
        samples,
    })
}

// ---------------------------------------------------------- JSON summary

fn build_json(results: &[CaseResult], oam_version: &str, commit: &str, profile: &str) -> String {
    let cases: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "median": r.median,
                "min": r.min,
                "max": r.max,
                "unit": r.unit,
                "iterations": r.iterations,
                "samples": r.samples,
            })
        })
        .collect();
    let summary = serde_json::json!({
        "schema": "oam-bench/1",
        "oamVersion": oam_version,
        "commit": commit,
        "profile": profile,
        "host": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "cases": cases,
    });
    serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "{}".to_string())
}

// -------------------------------------------------------- shared helpers
// Duplicated from conformance.rs (private there). Kept self-contained so
// bench.rs has no coupling beyond std + anyhow + serde_json.

fn repo_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("xtask manifest has a parent")?
        .to_path_buf())
}

fn ensure_oam_built(repo: &Path, release: bool) -> Result<PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let exe = repo
        .join(format!("target/{profile}"))
        .join(format!("oam{}", std::env::consts::EXE_SUFFIX));
    if !exe.is_file() {
        println!("building oam ({profile})...");
        let mut args = vec!["build", "-p", "oam_cli"];
        if release {
            args.push("--release");
        }
        let status = Command::new("cargo")
            .args(&args)
            .current_dir(repo)
            .status()?;
        if !status.success() {
            bail!("cargo build -p oam_cli failed");
        }
    }
    Ok(exe)
}

fn capture_version(exe: &Path, args: &[&str]) -> String {
    Command::new(exe)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn git_short_commit(repo: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

struct Captured {
    stdout: String,
    stderr: String,
    code: i32,
    timed_out: bool,
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Captured> {
    use std::io::Read;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {cmd:?}"))?;

    let mut stdout_pipe = child.stdout.take().context("stdout piped")?;
    let mut stderr_pipe = child.stderr.take().context("stderr piped")?;
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_pipe.read_to_string(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code().unwrap_or(-1);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break -2;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(Captured {
        stdout: stdout_thread.join().unwrap_or_default(),
        stderr: stderr_thread.join().unwrap_or_default(),
        code,
        timed_out,
    })
}
