//! `cargo run -p xtask -- node-suite`: run a vendored subset of Node's own
//! test suite under oam. Node core tests are SELF-ASSERTING (they
//! `require('../common')` + assert.* and exit non-zero on failure), so the
//! oracle is "exit 0 == pass" -- no per-test Node baseline needed (unlike the
//! byte-identical-stdout `conformance` differential suite).
//!
//! The vendored tree (conformance/vendor/node/) carries a `package.json` with
//! `{"type":"commonjs"}` so oam reads the bare `.js` files as CommonJS (oam
//! defaults typeless `.js` outside node_modules to ESM, where `require` is
//! undefined). An optional `manifest.json` marks per-test skips with reasons so
//! the denominator is auditable. Emits conformance/node-suite-scorecard.json +
//! CONFORMANCE-NODE.md (committed receipts), with per-module breakdown and BOTH
//! pass/runnable and pass/total rates so the runnable filter can't hide
//! exclusions.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::conformance::{Captured, ensure_oam_built, repo_root, run_with_timeout};

/// Modules vendored so far, longest-prefix-first so "string-decoder" wins over
/// a generic "string" split. Drives the per-module breakdown.
const MODULES: &[&str] = &[
    "string-decoder",
    "querystring",
    "buffer",
    "events",
    "assert",
    "util",
    "url",
    "path",
];

enum Outcome {
    Pass,
    Skip,
    Fail(String),
    Unrunnable(String),
}

pub(crate) fn run(release: bool) -> Result<()> {
    let repo = repo_root()?;
    let oam = ensure_oam_built(&repo, release)?;
    let cache = std::env::temp_dir().join(format!("oam-node-suite-{}", std::process::id()));
    std::fs::create_dir_all(&cache)?;

    let vendor = repo.join("conformance/vendor/node");
    let parallel_dir = vendor.join("test/parallel");
    if !parallel_dir.is_dir() {
        bail!(
            "vendored Node corpus not found at {} (run the vendor step first)",
            parallel_dir.display()
        );
    }

    let manifest = load_manifest(&vendor.join("manifest.json"));

    let mut tests: Vec<PathBuf> = std::fs::read_dir(&parallel_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("js") | Some("mjs")
            )
        })
        .collect();
    tests.sort();

    // per-module tallies: [pass, fail, skip, unrunnable]
    let mut by_module: BTreeMap<String, [usize; 4]> = BTreeMap::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    let (mut pass, mut fail, mut skip, mut unrunnable) = (0usize, 0usize, 0usize, 0usize);

    for test in &tests {
        let name = test
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let module = module_of(&name);

        let outcome = if let Some(reason) = manifest.get(&format!("parallel/{name}")) {
            Outcome::Unrunnable(reason.clone())
        } else if let Some(flags) = read_flags_header(test) {
            Outcome::Unrunnable(format!("// Flags:{flags}"))
        } else {
            run_test(&oam, test, &repo, &cache)
        };

        let slot = by_module.entry(module).or_insert([0; 4]);
        match outcome {
            Outcome::Pass => {
                pass += 1;
                slot[0] += 1;
                println!("  PASS   {name}");
            }
            Outcome::Skip => {
                skip += 1;
                slot[2] += 1;
                println!("  SKIP   {name}");
            }
            Outcome::Fail(detail) => {
                fail += 1;
                slot[1] += 1;
                println!("  FAIL   {name}  {detail}");
                failures.push((name.clone(), detail));
            }
            Outcome::Unrunnable(reason) => {
                unrunnable += 1;
                slot[3] += 1;
                println!("  UNRUN  {name}  ({reason})");
            }
        }
    }

    let total = tests.len();
    let runnable = pass + fail + skip;
    let scored = pass + fail; // skips excluded from the rate denominator
    let pct = |n: usize, d: usize| {
        if d == 0 {
            0.0
        } else {
            n as f64 * 100.0 / d as f64
        }
    };

    write_scorecard(
        &repo, &oam, &by_module, &failures, total, runnable, pass, fail, skip, unrunnable,
    )?;

    println!();
    println!("node-suite (vendored Node v22.22.2 subset):");
    println!(
        "  pass/runnable = {pass}/{scored} ({:.1}%)   pass/total = {pass}/{total} ({:.1}%)",
        pct(pass, scored),
        pct(pass, total)
    );
    println!("  {pass} pass  {fail} fail  {skip} skip  {unrunnable} unrunnable-by-harness");
    println!("wrote CONFORMANCE-NODE.md + conformance/node-suite-scorecard.json");
    Ok(())
}

/// Run one test, with a 3x flaky rerun: a Pass on any attempt wins; otherwise
/// the last Fail/Skip stands.
fn run_test(oam: &Path, test: &Path, repo: &Path, cache: &Path) -> Outcome {
    let mut last = Outcome::Fail("no attempt".to_string());
    for _ in 0..3 {
        let out = match run_with_timeout(
            Command::new(oam)
                .arg("run")
                .arg(test)
                .arg("--no-check")
                .env("OAM_CACHE_DIR", cache)
                .current_dir(repo),
            Duration::from_secs(60),
        ) {
            Ok(c) => c,
            Err(e) => return Outcome::Fail(format!("harness error: {e}")),
        };

        // common.skip() prints "1..0 # Skipped" and exits 0; the TAP marker is
        // authoritative over the exit code (else a skip inflates pass).
        let skipped = out
            .stdout
            .lines()
            .any(|l| l.contains("1..0 # Skipped") || l.contains("1..0 # SKIP"));
        if skipped {
            return Outcome::Skip;
        }
        if out.timed_out {
            last = Outcome::Fail("[TIMEOUT]".to_string());
            continue;
        }
        if out.code == 0 {
            return Outcome::Pass;
        }
        if let Some(builtin) = missing_builtin(&out) {
            return Outcome::Unrunnable(format!("needs unimplemented {builtin}"));
        }
        last = Outcome::Fail(format!("[exit={}] {}", out.code, first_error_line(&out)));
    }
    last
}

/// A test that fails purely because oam lacks a node: builtin it imports is
/// UNRUNNABLE-by-harness, not a correctness failure -- reclassify so the
/// denominator isn't unfairly depressed (e.g. node:test). Returns the builtin.
fn missing_builtin(out: &Captured) -> Option<String> {
    let hay = format!("{}\n{}", out.stderr, out.stdout);
    if let Some(idx) = hay.find("is not a known node: builtin module") {
        // oam: "'node:test' is not a known node: builtin module"
        let head = &hay[..idx];
        if let Some(q) = head.rfind('\'') {
            let pre = &head[..q];
            if let Some(q0) = pre.rfind('\'') {
                return Some(head[q0 + 1..q].to_string());
            }
        }
        return Some("a node: builtin".to_string());
    }
    if let Some(idx) = hay.find("Cannot find module 'node:") {
        let rest = &hay[idx + "Cannot find module '".len()..];
        if let Some(end) = rest.find('\'') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

fn module_of(name: &str) -> String {
    for m in MODULES {
        if name.starts_with(&format!("test-{m}-"))
            || name == format!("test-{m}.js")
            || name == format!("test-{m}.mjs")
        {
            return m.replace('-', "_");
        }
    }
    "other".to_string()
}

/// manifest.json: { "tests": { "parallel/test-x.js": { "skip": true, "reason": "..." } } }
fn load_manifest(path: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return map;
    };
    if let Some(tests) = v.get("tests").and_then(|t| t.as_object()) {
        for (k, cfg) in tests {
            if cfg.get("skip").and_then(|s| s.as_bool()) == Some(true) {
                let reason = cfg
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("manifest skip")
                    .to_string();
                map.insert(k.clone(), reason);
            }
        }
    }
    map
}

/// Read the `// Flags: ...` preamble (Node scans the first ~1.5KB).
fn read_flags_header(path: &Path) -> Option<String> {
    let src = std::fs::read_to_string(path).ok()?;
    for line in src.lines().take(40) {
        if let Some(rest) = line.trim_start().strip_prefix("// Flags:") {
            return Some(rest.to_string());
        }
    }
    None
}

fn first_error_line(out: &Captured) -> String {
    let pick = |s: &str| {
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|l| l.chars().take(140).collect::<String>())
    };
    pick(&out.stderr)
        .or_else(|| pick(&out.stdout))
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn write_scorecard(
    repo: &Path,
    oam: &Path,
    by_module: &BTreeMap<String, [usize; 4]>,
    failures: &[(String, String)],
    total: usize,
    runnable: usize,
    pass: usize,
    fail: usize,
    skip: usize,
    unrunnable: usize,
) -> Result<()> {
    let oam_version = Command::new(oam)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let scored = pass + fail;
    let pct = |n: usize, d: usize| {
        if d == 0 {
            0.0
        } else {
            n as f64 * 100.0 / d as f64
        }
    };

    let modules_json: Value = by_module
        .iter()
        .map(|(m, c)| {
            (
                m.clone(),
                json!({ "pass": c[0], "fail": c[1], "skip": c[2], "unrunnable": c[3] }),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into();

    let scorecard = json!({
        "schema": "oam-node-suite/1",
        "commit": commit,
        "oamVersion": oam_version,
        "nodeVersion": "v22.22.2",
        "host": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "corpus": "test/parallel (pure non-I/O core modules: buffer, events, assert, util, querystring, string_decoder, url, path)",
        "total": total,
        "runnable": runnable,
        "pass": pass,
        "fail": fail,
        "skip": skip,
        "unrunnable": unrunnable,
        "passOverRunnable": format!("{:.1}%", pct(pass, scored)),
        "passOverTotal": format!("{:.1}%", pct(pass, total)),
        "byModule": modules_json,
    });
    std::fs::write(
        repo.join("conformance/node-suite-scorecard.json"),
        serde_json::to_string_pretty(&scorecard)?,
    )?;

    let mut md = String::new();
    md.push_str("# oam Node-suite scorecard\n\n");
    md.push_str("Generated by `cargo run -p xtask -- node-suite` -- do not edit by hand.\n");
    md.push_str("Machine twin: [`conformance/node-suite-scorecard.json`](conformance/node-suite-scorecard.json).\n\n");
    md.push_str(&format!(
        "Commit `{commit}` | {oam_version} | Node v22.22.2 | host {}-{}\n\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    md.push_str("Oracle: a vendored Node core test passes when it runs to **exit 0** (Node tests self-assert via `require('../common')` + `assert`). `1..0 # Skipped` reclassifies a runtime skip; `// Flags:` and manifest entries are unrunnable-by-harness.\n\n");
    md.push_str(&format!(
        "**pass/runnable = {pass}/{scored} ({:.1}%)** &nbsp; pass/total = {pass}/{total} ({:.1}%)\n\n",
        pct(pass, scored),
        pct(pass, total)
    ));
    md.push_str(&format!(
        "{pass} pass &middot; {fail} fail &middot; {skip} skip &middot; {unrunnable} unrunnable-by-harness\n\n"
    ));
    md.push_str("## By module\n\n");
    md.push_str(
        "| module | pass | fail | skip | unrun | pass/runnable |\n|---|---|---|---|---|---|\n",
    );
    for (m, c) in by_module {
        let s = c[0] + c[1];
        md.push_str(&format!(
            "| {m} | {} | {} | {} | {} | {:.0}% |\n",
            c[0],
            c[1],
            c[2],
            c[3],
            pct(c[0], s)
        ));
    }
    md.push_str("\nCorpus: a pure non-I/O core-module cross-section of `test/parallel` (slice 2). I/O modules (fs, net, stream, child_process, http) and the full corpus + an out-of-band CI run land in later slices.\n");
    if !failures.is_empty() {
        md.push_str("\n## Failures (first divergence, triage backlog)\n\n");
        for (name, detail) in failures {
            md.push_str(&format!("- `{name}` -- {detail}\n"));
        }
    }
    std::fs::write(repo.join("CONFORMANCE-NODE.md"), md)?;
    Ok(())
}
