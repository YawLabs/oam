//! `cargo run -p xtask -- node-suite`: run a vendored subset of Node's own
//! test suite under oam. Node core tests are SELF-ASSERTING (they
//! `require('../common')` + assert.* and exit non-zero on failure), so the
//! oracle is "exit 0 == pass" -- no per-test Node baseline needed (unlike the
//! byte-identical-stdout `conformance` differential suite).
//!
//! Slice 1 (de-risking): runs everything vendored under
//! conformance/vendor/node/test/parallel/. The vendored tree carries a
//! `package.json` with `{"type":"commonjs"}` so oam reads the bare `.js`
//! files as CommonJS (oam defaults typeless `.js` outside node_modules to
//! ESM, where `require` is undefined). Scorecard/CI wiring + a manifest land
//! in later slices.

use anyhow::{Result, bail};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::conformance::{Captured, ensure_oam_built, repo_root, run_with_timeout};

pub(crate) fn run(release: bool) -> Result<()> {
    let repo = repo_root()?;
    let oam = ensure_oam_built(&repo, release)?;
    let cache = std::env::temp_dir().join(format!("oam-node-suite-{}", std::process::id()));
    std::fs::create_dir_all(&cache)?;

    let parallel_dir = repo.join("conformance/vendor/node/test/parallel");
    if !parallel_dir.is_dir() {
        bail!(
            "vendored Node corpus not found at {} (run the vendor step first)",
            parallel_dir.display()
        );
    }

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

    let (mut pass, mut fail, mut skip, mut unrunnable) = (0usize, 0usize, 0usize, 0usize);
    let mut failures: Vec<(String, i32, String)> = Vec::new();

    for test in &tests {
        let name = test
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // `// Flags:` header -> the test needs CLI flags oam's `run` doesn't
        // accept. UNRUNNABLE-by-harness, not a fail. (Slice 1: any flag line
        // is out of scope; mapping benign flags lands in a later slice.)
        if let Some(flags) = read_flags_header(test) {
            unrunnable += 1;
            println!("  UNRUN {name}  (// Flags:{flags})");
            continue;
        }

        let out = run_with_timeout(
            Command::new(&oam)
                .arg("run")
                .arg(test)
                .arg("--no-check")
                .env("OAM_CACHE_DIR", &cache)
                .current_dir(&repo),
            Duration::from_secs(60),
        )?;

        // common.skip() prints "1..0 # Skipped" and exits 0; that TAP marker
        // is authoritative over the exit code, else a skip inflates pass.
        let skipped = out
            .stdout
            .lines()
            .any(|l| l.contains("1..0 # Skipped") || l.contains("1..0 # SKIP"));

        if out.timed_out {
            fail += 1;
            failures.push((name.clone(), -2, "TIMEOUT".to_string()));
            println!("  FAIL  {name}  [TIMEOUT]");
        } else if skipped {
            skip += 1;
            println!("  SKIP  {name}");
        } else if out.code == 0 {
            pass += 1;
            println!("  PASS  {name}");
        } else {
            fail += 1;
            let detail = first_error_line(&out);
            println!("  FAIL  {name}  [exit={}]  {detail}", out.code);
            failures.push((name.clone(), out.code, detail));
        }
    }

    let attempted = pass + fail + skip;
    println!();
    println!(
        "node-suite (vendored Node subset): {pass}/{attempted} pass  |  {skip} skip  {fail} fail  {unrunnable} unrunnable-by-harness"
    );
    Ok(())
}

/// Read the `// Flags: ...` preamble (Node scans the first ~1.5KB). Returns
/// the text after `// Flags:` if present.
fn read_flags_header(path: &std::path::Path) -> Option<String> {
    let src = std::fs::read_to_string(path).ok()?;
    for line in src.lines().take(40) {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("// Flags:") {
            return Some(rest.to_string());
        }
    }
    None
}

/// First meaningful error line for triage (stderr preferred, else stdout).
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
