//! `cargo xtask bench`: micro-benchmark harness for the oam JS runtime.
//!
//! Nine benchmark cases measure different layers of the stack:
//!   1. cold-start             -- process start to exit on a trivial script
//!   2. url-parse              -- URL constructor throughput (10k parses)
//!   3. http-throughput        -- node:http + fetch loopback (200 req)
//!   4. fs-read                -- fs.readFileSync hot-loop (1000 reads of 4KB)
//!   5. json-parse             -- JSON.parse + JSON.stringify (1000 round-trips)
//!   6. crypto-hash            -- SHA-256 hash of 64KB (1000 iterations)
//!   7. mcp-cold-start         -- process spawn to first MCP initialize response
//!   8. mcp-idle-rss           -- RSS (MB) after initialize, server idle
//!   9. mcp-first-call-latency -- tools/call round-trip on warm server
//!
//! With `--compare`, the same scripts run under every runtime found on
//! PATH (node, bun, deno) and the harness reports a comparison table.
//!
//! Outputs: bench/results.json (machine) and BENCHMARKS.md (human),
//! both COMMITTED -- the receipt is in the repo.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ config

const COLD_START_ITERS: usize = 10;
const TIMED_ITERS: usize = 5;
const HTTP_ITERS: usize = 3;
const CASE_TIMEOUT: Duration = Duration::from_secs(120);

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
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&path_var) {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!("{name} not found on PATH");
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

fn run_timed_case(
    case_name: &str,
    rt: &Runtime,
    script: &Path,
    iters: usize,
    extra_args: &[String],
) -> Result<CaseResult> {
    print!("  {}/{case_name}: ", rt.name);
    let mut samples = Vec::with_capacity(iters);
    for i in 0..iters {
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
        let value = v["elapsed_ms"].as_f64().context("missing elapsed_ms")?;
        samples.push(value);
        print!(".");
    }
    println!(" done");

    let (p50, min, max, p95, p99) = stats(&samples);
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
        iterations: iters,
        warmup_iterations: 0,
        samples,
    })
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

/// MCP cold-start: time from process spawn to the first JSON-RPC response.
///
/// Spawns the server script, sends an MCP `initialize` request via stdin,
/// and records wall-clock time until a JSON-RPC response line arrives on
/// stdout. Kills the server after each measurement.
fn run_mcp_cold_start(rt: &Runtime, server_script: &Path) -> Result<CaseResult> {
    use std::io::{BufRead, Write};

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
        let mut cmd = rt.build_cmd(server_script, &[]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let start = Instant::now();
        let mut child = cmd.spawn().context("spawn MCP server")?;

        // Write the initialize request. Keep stdin open (do NOT drop the handle
        // yet): closing stdin sends EOF, which the server sees as "no more input"
        // and may exit before writing its response.
        let mut stdin = child.stdin.take().context("mcp server stdin")?;
        stdin.write_all(init_msg.as_bytes())?;
        stdin.flush()?;

        // Read lines until we get a non-empty JSON line (the initialize response).
        let stdout = child.stdout.take().context("mcp server stdout")?;
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let elapsed = loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                bail!(
                    "{}/mcp-cold-start iter {i}: server closed stdout before response",
                    rt.name
                );
            }
            if !line.trim().is_empty() {
                break start.elapsed().as_secs_f64() * 1000.0;
            }
        };
        drop(stdin); // close stdin AFTER reading the response

        let _ = child.kill();
        let _ = child.wait();
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
        // Find the last quoted field (mem usage) e.g. "12,345 K"
        let field = text.split(',').rev().take(2).collect::<Vec<_>>().concat();
        let digits: String = field.chars().filter(|c| c.is_ascii_digit()).collect();
        let kb: u64 = digits.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(target_os = "windows"))]
    {
        // /proc/<pid>/status VmRSS line: "VmRSS:   12345 kB"
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
}

/// MCP idle RSS: spawn server, complete `initialize`, sample RSS, kill.
/// Unit: MB (megabytes). The initialize round-trip ensures the runtime is
/// fully loaded (module graph evaluated, event loop alive) before sampling.
fn run_mcp_idle_rss(rt: &Runtime, server_script: &Path) -> Result<CaseResult> {
    use std::io::{BufRead, Write};

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

    for _i in 0..iters {
        let mut cmd = rt.build_cmd(server_script, &[]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("spawn MCP server")?;
        let pid = child.id();

        let mut stdin = child.stdin.take().context("mcp server stdin")?;
        stdin.write_all(init_msg.as_bytes())?;
        stdin.flush()?;

        let stdout = child.stdout.take().context("mcp server stdout")?;
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if !line.trim().is_empty() {
                break;
            }
        }

        // Server is now idle (waiting for next request) -- sample RSS.
        if let Some(bytes) = process_rss_bytes(pid) {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            samples.push(mb);
        }

        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
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
    use std::io::{BufRead, Write};

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
        let mut cmd = rt.build_cmd(server_script, &[]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("spawn MCP server")?;

        let mut stdin = child.stdin.take().context("mcp server stdin")?;
        stdin.write_all(init_msg.as_bytes())?;
        stdin.flush()?;

        let stdout = child.stdout.take().context("mcp server stdout")?;
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();

        // Drain the initialize response.
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                bail!(
                    "{}/mcp-first-call-latency iter {i}: closed before initialize response",
                    rt.name
                );
            }
            if !line.trim().is_empty() {
                break;
            }
        }

        // Send the initialized notification (required by MCP spec before tools/call).
        stdin.write_all(initialized_notif.as_bytes())?;
        stdin.flush()?;

        // Now measure: send tools/call, time to response.
        let call_start = Instant::now();
        stdin.write_all(call_msg.as_bytes())?;
        stdin.flush()?;

        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                bail!(
                    "{}/mcp-first-call-latency iter {i}: closed before tools/call response",
                    rt.name
                );
            }
            if !line.trim().is_empty() {
                break;
            }
        }
        let elapsed_ms = call_start.elapsed().as_secs_f64() * 1000.0;
        samples.push(elapsed_ms);

        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
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

// ------------------------------------------------------------------ entry

pub fn run(release: bool, compare: bool) -> Result<()> {
    let repo = repo_root()?;
    let oam = ensure_oam_built(&repo, release)?;
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

    let case_names = [
        "cold-start",
        "url-parse",
        "http-throughput",
        "fs-read",
        "json-parse",
        "crypto-hash",
        "mcp-cold-start",
        "mcp-idle-rss",
        "mcp-first-call-latency",
    ];

    let scripts = write_scripts(&tmp, &repo)?;
    let mut all_results: Vec<CaseResult> = Vec::new();

    for case_name in &case_names {
        println!("{case_name}:");
        for rt in &runtimes {
            let result = match *case_name {
                "cold-start" => run_cold_start(rt, &scripts.cold_start),
                "url-parse" => run_timed_case(case_name, rt, &scripts.url_parse, TIMED_ITERS, &[]),
                "http-throughput" => {
                    run_timed_case(case_name, rt, &scripts.http_throughput, HTTP_ITERS, &[])
                }
                "fs-read" => run_timed_case(
                    case_name,
                    rt,
                    &scripts.fs_read,
                    TIMED_ITERS,
                    &[scripts.data_file.to_string_lossy().to_string()],
                ),
                "json-parse" => {
                    run_timed_case(case_name, rt, &scripts.json_parse, TIMED_ITERS, &[])
                }
                "crypto-hash" => {
                    run_timed_case(case_name, rt, &scripts.crypto_hash, TIMED_ITERS, &[])
                }
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
                    println!("  {}/{case_name}: FAILED -- {e}", rt.name);
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);

    // ------------------------------------------------------------- table
    print_table(&all_results, &runtimes, &case_names);

    // --------------------------------------------------------- write files
    let json = build_json(&all_results, &runtimes, &commit, profile);
    let bench_dir = repo.join("bench");
    std::fs::create_dir_all(&bench_dir)?;
    let results_path = bench_dir.join("results.json");
    std::fs::write(&results_path, serde_json::to_string_pretty(&json)?)?;

    let md = build_markdown(&all_results, &runtimes, &case_names, &commit, profile);
    std::fs::write(repo.join("BENCHMARKS.md"), md)?;

    println!("wrote BENCHMARKS.md + bench/results.json");
    Ok(())
}

// ------------------------------------------------------------ scripts

struct Scripts {
    cold_start: PathBuf,
    url_parse: PathBuf,
    http_throughput: PathBuf,
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
                        print!(" {:>9.2}ms", r.median);
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

// -------------------------------------------------------------- JSON output

fn build_json(
    results: &[CaseResult],
    runtimes: &[Runtime],
    commit: &str,
    profile: &str,
) -> serde_json::Value {
    let rt_info: Vec<serde_json::Value> = runtimes
        .iter()
        .map(|rt| {
            serde_json::json!({
                "name": rt.name,
                "version": rt.version,
                "exe": rt.exe.to_string_lossy(),
            })
        })
        .collect();

    let cases: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
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
            })
        })
        .collect();

    let timestamp = chrono_utc_now();

    serde_json::json!({
        "schema": "oam-bench/3",
        "timestamp": timestamp,
        "commit": commit,
        "ci_run_id": std::env::var("GITHUB_RUN_ID").unwrap_or_default(),
        "host": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "hardware_class": std::env::var("BENCH_HARDWARE_CLASS").unwrap_or_else(|_| "unknown".to_string()),
        "profile": profile,
        "warmup_iterations": 0,
        "runtimes": rt_info,
        "results": cases,
    })
}

// ---------------------------------------------------------- markdown output

fn build_markdown(
    results: &[CaseResult],
    runtimes: &[Runtime],
    case_names: &[&str],
    commit: &str,
    profile: &str,
) -> String {
    let mut md = String::new();
    md.push_str("# Benchmarks\n\n");
    md.push_str("Generated by `cargo xtask bench` -- do not edit by hand.\n");
    md.push_str("Machine-readable twin: [`bench/results.json`](bench/results.json).\n\n");
    md.push_str(&format!(
        "Commit `{commit}` | {profile} | host {}-{}\n\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

    md.push_str("## Runtimes\n\n");
    for rt in runtimes {
        md.push_str(&format!("- **{}** {}\n", rt.name, rt.version));
    }
    md.push('\n');

    md.push_str("## Results\n\n");
    md.push_str("All times in milliseconds. Lower is better.\n\n");

    if runtimes.len() == 1 {
        md.push_str("| Case | Median | Min | Max |\n");
        md.push_str("|---|--:|--:|--:|\n");
        for r in results {
            md.push_str(&format!(
                "| {} | {:.2} | {:.2} | {:.2} |\n",
                r.name, r.median, r.min, r.max
            ));
        }
    } else {
        md.push_str("| Case |");
        for rt in runtimes {
            md.push_str(&format!(" {} |", rt.name));
        }
        if runtimes.len() >= 2 {
            md.push_str(&format!(" vs {} |", runtimes[1].name));
        }
        md.push_str("\n|---|");
        for _ in runtimes {
            md.push_str("--:|");
        }
        if runtimes.len() >= 2 {
            md.push_str("--:|");
        }
        md.push('\n');

        for case_name in case_names {
            md.push_str(&format!("| {case_name} |"));
            let mut medians: Vec<Option<f64>> = Vec::new();
            for rt in runtimes {
                let result = results
                    .iter()
                    .find(|r| r.name == *case_name && r.runtime == rt.name);
                match result {
                    Some(r) => {
                        md.push_str(&format!(" {:.2} |", r.median));
                        medians.push(Some(r.median));
                    }
                    None => {
                        md.push_str(" -- |");
                        medians.push(None);
                    }
                }
            }
            if medians.len() >= 2 {
                if let (Some(oam), Some(baseline)) = (medians[0], medians[1]) {
                    let ratio = oam / baseline;
                    md.push_str(&format!(" {:.2}x |", ratio));
                } else {
                    md.push_str(" -- |");
                }
            }
            md.push('\n');
        }
    }

    md.push_str("\n## Cases\n\n");
    md.push_str("- **cold-start** -- wall-clock time from process spawn to exit (`console.log('ok')`). Measured by the harness, not JS.\n");
    md.push_str(
        "- **url-parse** -- 10,000 `new URL()` constructions across 5 representative URLs.\n",
    );
    md.push_str("- **http-throughput** -- node:http server + fetch client, 200 sequential requests on loopback.\n");
    md.push_str("- **fs-read** -- `fs.readFileSync` of a 4KB file, 1,000 iterations.\n");
    md.push_str("- **json-parse** -- `JSON.parse(JSON.stringify(obj))` round-trip on a 100-user payload, 1,000 iterations.\n");
    md.push_str("- **crypto-hash** -- `crypto.createHash('sha256').update(64KB).digest()`, 1,000 iterations.\n");
    md.push_str("- **mcp-cold-start** -- wall-clock from process spawn to the first MCP `initialize` response via stdio. oam uses the built-in `oam:mcp` virtual module; other runtimes use `@modelcontextprotocol/sdk` (same noop tool, same transport).\n");
    md.push_str("- **mcp-idle-rss** -- resident set size (MB) after `initialize` completes and the server is idle, waiting for the next request. Measures runtime memory overhead of a hosted MCP server.\n");
    md.push_str("- **mcp-first-call-latency** -- wall-clock from sending `tools/call` to receiving the response, on an already-initialized server. Measures warm-path dispatch latency (no cold-start cost).\n");

    md
}

// -------------------------------------------------------- shared helpers

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

/// ISO 8601 UTC timestamp via shell `date`, no chrono dependency.
fn chrono_utc_now() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
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
