//! `cargo run -p xtask -- conformance`: the reliability-with-receipts
//! harness. Three suites, one scorecard:
//!
//! 1. wpt-url — the OFFICIAL web-platform-tests URL data (vendored
//!    snapshot under conformance/vendor/wpt/url) run against oam's URL.
//! 2. node-differential — every conformance/cases/*.mjs executed under
//!    BOTH oam and the installed Node; a case passes when stdout and exit
//!    code are byte-identical.
//! 3. surface — which Node builtin modules load and which globals exist.
//!
//! Outputs: conformance/scorecard.json (machine) and CONFORMANCE.md
//! (humans), both COMMITTED — the receipt is in the repo, and CI
//! regenerates it on every push.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn run(release: bool) -> Result<()> {
    let release = release
        || std::env::var("CONFORMANCE_RELEASE")
            .map(|v| v == "1")
            .unwrap_or(false);
    let repo = repo_root()?;
    let oam = ensure_oam_built(&repo, release)?;
    let node = which_node();
    let cache = std::env::temp_dir().join(format!("oam-conformance-{}", std::process::id()));
    std::fs::create_dir_all(&cache)?;

    let oam_version = capture_version(&oam, &["--version"]);
    let node_version = node
        .as_deref()
        .map(|node| capture_version(Path::new(node), &["--version"]))
        .unwrap_or_else(|| "absent".to_string());

    // oam_version already carries the "oam " program-name prefix from
    // `oam --version`; don't prepend another (was "oam oam 0.0.1").
    println!("{oam_version} vs node {node_version}");

    // ------------------------------------------------------------ wpt-url
    println!("suite: wpt-url");
    let vendor = repo.join("conformance/vendor/wpt/url");
    let runner = repo.join("conformance/runners/wpt_url.mjs");
    let output = run_with_timeout(
        Command::new(&oam)
            .arg("run")
            .arg(&runner)
            .arg("--no-check")
            .arg("--")
            .arg(&vendor)
            .env("OAM_CACHE_DIR", &cache)
            .current_dir(&repo),
        Duration::from_secs(120),
    )?;
    let wpt: Value = serde_json::from_str(output.stdout.trim()).with_context(|| {
        format!(
            "wpt_url runner produced unparseable output; stderr: {}",
            output.stderr
        )
    })?;

    // ------------------------------------------------------------ surface
    println!("suite: surface");
    let surface_runner = repo.join("conformance/runners/surface.mjs");
    let output = run_with_timeout(
        Command::new(&oam)
            .arg("run")
            .arg(&surface_runner)
            .arg("--no-check")
            .env("OAM_CACHE_DIR", &cache)
            .current_dir(&repo),
        Duration::from_secs(60),
    )?;
    let surface: Value = serde_json::from_str(output.stdout.trim())
        .with_context(|| format!("surface runner failed; stderr: {}", output.stderr))?;
    let module_count = |v: &Value| {
        let map = v.as_object().cloned().unwrap_or_default();
        let total = map.len();
        let have = map.values().filter(|x| x.as_bool() == Some(true)).count();
        (have, total)
    };
    let (modules_have, modules_total) = module_count(&surface["modules"]);
    let (globals_have, globals_total) = module_count(&surface["globals"]);

    // --------------------------------------------------- node-differential
    println!("suite: node-differential");
    let cases_dir = repo.join("conformance/cases");
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&cases_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mjs"))
        .collect();
    cases.sort();

    let mut diff_results = Vec::new();
    let mut diff_pass = 0usize;
    for case in &cases {
        let name = case
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(node) = node.as_deref() else {
            diff_results
                .push(json!({ "case": name, "status": "skipped", "reason": "node absent" }));
            continue;
        };
        let oam_out = run_with_timeout(
            Command::new(&oam)
                .arg("run")
                .arg(case)
                .arg("--no-check")
                .env("OAM_CACHE_DIR", &cache)
                .current_dir(&repo),
            Duration::from_secs(60),
        )?;
        let node_out = run_with_timeout(
            Command::new(node).arg(case).current_dir(&repo),
            Duration::from_secs(60),
        )?;
        if oam_out.timed_out || node_out.timed_out {
            println!("  TIMEOUT {name}");
            diff_results.push(json!({ "case": name, "status": "timeout" }));
            continue;
        }
        let same_stdout = normalize(&oam_out.stdout) == normalize(&node_out.stdout);
        let same_exit = oam_out.code == node_out.code;
        if same_stdout && same_exit {
            diff_pass += 1;
            diff_results.push(json!({ "case": name, "status": "pass" }));
            println!("  pass {name}");
        } else {
            let detail =
                first_difference(&normalize(&oam_out.stdout), &normalize(&node_out.stdout));
            println!("  FAIL {name}");
            diff_results.push(json!({
                "case": name,
                "status": "fail",
                "exit": { "oam": oam_out.code, "node": node_out.code },
                "firstDifference": detail,
                "oamStderr": oam_out.stderr.lines().take(3).collect::<Vec<_>>().join(" | "),
            }));
        }
    }
    let diff_total = cases.len();

    // ---------------------------------------------------------- scorecard
    let commit = git_short_commit(&repo);
    let scorecard = json!({
        "schema": "oam-conformance/1",
        "commit": commit,
        "oamVersion": oam_version,
        "nodeVersion": node_version,
        "host": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "suites": {
            "wptUrl": {
                "constructor": wpt["constructor"],
                "setters": wpt["setters"],
                "failureCount": wpt["failureCount"],
                "failureSample": wpt["failureSample"],
                "source": "web-platform-tests url/resources (vendored snapshot; see conformance/vendor/wpt/url/README.md)",
            },
            "nodeDifferential": {
                "pass": diff_pass,
                "total": diff_total,
                "cases": diff_results,
            },
            "surface": {
                "modules": { "have": modules_have, "total": modules_total, "detail": surface["modules"] },
                "globals": { "have": globals_have, "total": globals_total, "detail": surface["globals"] },
            },
        },
    });
    let scorecard_path = repo.join("conformance/scorecard.json");
    std::fs::write(&scorecard_path, serde_json::to_string_pretty(&scorecard)?)?;

    // ---------------------------------------------------------- dashboard
    let ctor_pass = wpt["constructor"]["pass"].as_u64().unwrap_or(0);
    let ctor_total = wpt["constructor"]["total"].as_u64().unwrap_or(0);
    let set_pass = wpt["setters"]["pass"].as_u64().unwrap_or(0);
    let set_total = wpt["setters"]["total"].as_u64().unwrap_or(0);
    let pct = |p: u64, t: u64| {
        if t == 0 {
            0.0
        } else {
            (p as f64) * 100.0 / (t as f64)
        }
    };

    let mut md = String::new();
    md.push_str("# oam conformance scorecard\n\n");
    md.push_str("Generated by `cargo run -p xtask -- conformance` — do not edit by hand.\n");
    md.push_str(
        "Machine-readable twin: [`conformance/scorecard.json`](conformance/scorecard.json).\n\n",
    );
    md.push_str(&format!(
        "Commit `{commit}` | {oam_version} | node {node_version} | host {}-{}\n\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    md.push_str("## WPT: URL (official web-platform-tests data)\n\n");
    md.push_str("| suite | pass | total | % |\n|---|---|---|---|\n");
    md.push_str(&format!(
        "| URL constructor | {ctor_pass} | {ctor_total} | {:.1}% |\n",
        pct(ctor_pass, ctor_total)
    ));
    md.push_str(&format!(
        "| URL setters | {set_pass} | {set_total} | {:.1}% |\n\n",
        pct(set_pass, set_total)
    ));
    md.push_str("## Node differential (same script, both runtimes, byte-identical stdout)\n\n");
    md.push_str(&format!(
        "**{diff_pass} / {diff_total} cases identical to Node {node_version}.**\n\n"
    ));
    md.push_str("| case | status |\n|---|---|\n");
    for result in &diff_results {
        md.push_str(&format!(
            "| {} | {} |\n",
            result["case"].as_str().unwrap_or("?"),
            result["status"].as_str().unwrap_or("?")
        ));
    }
    md.push_str("\n## Builtin surface\n\n");
    md.push_str(&format!(
        "- Node builtin modules loading: **{modules_have} / {modules_total}**\n"
    ));
    md.push_str(&format!(
        "- Runtime globals present: **{globals_have} / {globals_total}**\n\n"
    ));
    md.push_str("Surface is breadth, not depth — the differential and WPT suites measure depth.\n");
    md.push_str("Per-name detail lives in the JSON scorecard.\n");
    std::fs::write(repo.join("CONFORMANCE.md"), md)?;

    println!();
    println!(
        "wpt-url: constructor {ctor_pass}/{ctor_total} ({:.1}%), setters {set_pass}/{set_total} ({:.1}%)",
        pct(ctor_pass, ctor_total),
        pct(set_pass, set_total)
    );
    println!("node-differential: {diff_pass}/{diff_total}");
    println!(
        "surface: modules {modules_have}/{modules_total}, globals {globals_have}/{globals_total}"
    );
    println!("wrote CONFORMANCE.md + conformance/scorecard.json");
    Ok(())
}

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

fn which_node() -> Option<String> {
    let probe = Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match probe {
        Ok(status) if status.success() => Some("node".to_string()),
        _ => None,
    }
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

/// Spawn with piped output and a hard deadline (the oam_mcp try_wait
/// pattern): a hung case must fail the suite, not the harness.
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
            break -2; // timeout sentinel
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

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
}

fn first_difference(a: &str, b: &str) -> Value {
    for (index, (left, right)) in a.lines().zip(b.lines()).enumerate() {
        if left != right {
            return json!({ "line": index + 1, "oam": left, "node": right });
        }
    }
    let (al, bl) = (a.lines().count(), b.lines().count());
    if al != bl {
        return json!({ "line": al.min(bl) + 1, "oam": format!("<{al} lines>"), "node": format!("<{bl} lines>") });
    }
    json!(null)
}
