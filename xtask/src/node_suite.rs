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
    "child-process",
    "buffer",
    "events",
    "assert",
    "stream",
    "timers",
    "process",
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

        let outcome = if let Some(reason) = manifest.skips.get(&format!("parallel/{name}")) {
            Outcome::Unrunnable(reason.clone())
        } else {
            // A `// Flags:` header is only disqualifying when oam cannot
            // honor the flags. Excluding EVERY flagged test shrank the
            // denominator with tests oam runs correctly -- a measurement
            // that flatters the runtime for free. Pass through what we
            // support; exclude only the genuinely unsupported.
            match classify_flags(test) {
                FlagSupport::None => run_test(&oam, test, &vendor, &cache, &[]),
                FlagSupport::Supported(flags) => run_test(&oam, test, &vendor, &cache, &flags),
                FlagSupport::Unsupported(raw) => Outcome::Unrunnable(format!("// Flags:{raw}")),
            }
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
    // The rate denominator is SCORED tests -- a self-skipping test produced no
    // verdict, so counting it would let a platform look better by skipping
    // more. `skip` and `unrunnable` stay in the scorecard beside it so nothing
    // is hidden; what must never happen is the published count and the
    // published percentage disagreeing about which denominator they used.
    let scored = pass + fail;
    let pct = |n: usize, d: usize| {
        if d == 0 {
            0.0
        } else {
            n as f64 * 100.0 / d as f64
        }
    };

    write_scorecard(
        &repo, &oam, &by_module, &failures, total, scored, pass, fail, skip, unrunnable,
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

    // Skip-ratchet: the discretionary manifest skip count must stay within the
    // committed ceilings. Raising a ceiling is a reviewable diff; exceeding it
    // fails the run so CI catches skip-inflation of the denominator. Auto-
    // detected unrunnables (// Flags, missing node: builtin) are NOT counted.
    let manifest_skips = manifest.skips.len();
    if let Some(max) = manifest.max_skips {
        println!(
            "manifest skips: {manifest_skips}/{max} (ratchet ceiling), known-issues {}/{}",
            manifest.known_issues,
            manifest
                .max_known_issues
                .map(|m| m.to_string())
                .unwrap_or_else(|| "-".into())
        );
    }
    // Per-host pass floor when calibrated (pass counts differ by platform --
    // the 270 floor measured on windows-aarch64 is not linux-x86_64's honest
    // number); the global minPass covers hosts without an entry.
    let host = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let min_pass = manifest
        .min_pass_by_host
        .get(&host)
        .copied()
        .or(manifest.min_pass);
    if let Some(msg) = ratchet_violation(
        manifest_skips,
        manifest.known_issues,
        pass,
        manifest.max_skips,
        manifest.max_known_issues,
        min_pass,
    ) {
        bail!("{msg}");
    }
    Ok(())
}

/// Pure ratchet check (extracted for unit testing): returns Some(message) when
/// the discretionary skip count or known-issues count exceeds its ceiling, OR
/// the pass count falls below its floor. None = all within limits / unset.
/// `== ceiling` and `== floor` are allowed; only strictly past them violates.
fn ratchet_violation(
    skips: usize,
    known_issues: usize,
    pass: usize,
    max_skips: Option<usize>,
    max_known_issues: Option<usize>,
    min_pass: Option<usize>,
) -> Option<String> {
    if let Some(max) = max_skips
        && skips > max
    {
        return Some(format!(
            "skip-ratchet violation: {skips} manifest skips > ceiling {max}. \
             Fix the tests, or (with review) raise ratchet.maxSkips in manifest.json -- it should only ever go DOWN."
        ));
    }
    if let Some(max) = max_known_issues
        && known_issues > max
    {
        return Some(format!(
            "known-issues ceiling violation: {known_issues} known_issues/flaky skips > ceiling {max}."
        ));
    }
    if let Some(min) = min_pass
        && pass < min
    {
        return Some(format!(
            "pass-floor violation: {pass} passing < floor {min}. A node-compat regression \
             dropped the pass count. Fix it, or (with review) lower ratchet.minPass -- it should only ever go UP."
        ));
    }
    None
}

/// Run one test, with a 3x flaky rerun: a Pass on any attempt wins; otherwise
/// the last Fail/Skip stands.
///
/// `cwd` is the VENDOR root, not the oam repo: Node runs its suite from the
/// Node repo root, and the vendored tree is our stand-in for it (same
/// `test/`, `lib/`, package.json layout). A handful of tests read
/// `process.cwd()` directly.
fn run_test(oam: &Path, test: &Path, cwd: &Path, cache: &Path, flags: &[String]) -> Outcome {
    let mut last = Outcome::Fail("no attempt".to_string());
    for _ in 0..3 {
        // Node-level flags go BEFORE the file, in the bare `oam <flags>
        // <file>` form -- that is the shape oam's node-style parser reads,
        // and it is what `node --no-warnings test.js` looks like.
        let mut cmd = Command::new(oam);
        for f in flags {
            cmd.arg(f);
        }
        if flags.is_empty() {
            cmd.arg("run").arg(test).arg("--no-check");
        } else {
            cmd.arg(test);
        }
        let out = match run_with_timeout(
            cmd.env("OAM_CACHE_DIR", cache).current_dir(cwd),
            // 60s was below the honest debug-build runtime of the slowest
            // vendored tests (test-util-inspect-long-running takes ~90s here),
            // which made their result depend on machine load rather than on
            // correctness. Still bounded, so a genuine hang is still caught.
            Duration::from_secs(150),
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
    // Same class for an `internal/...` module oam does not provide under
    // --expose-internals (Node C++ bindings, libuv stream wrappers). The
    // test never reaches an assertion about oam's behavior, so counting it
    // as a correctness failure would be as wrong as counting a missing
    // node: builtin -- and counting it as a PASS would be worse.
    if let Some(idx) = hay.find("Cannot find module 'internal/") {
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

/// manifest.json:
/// {
///   "ratchet": { "maxSkips": N, "maxKnownIssues": M, "minPass": K,
///                "minPassByHost": { "<os>-<arch>": K2, ... } },
///   "tests": { "parallel/test-x.js": { "skip": true, "reason": "...", "category": "known_issues" } }
/// }
///
/// minPassByHost overrides minPass for a specific `{OS}-{ARCH}` host label
/// (e.g. "linux-x86_64") -- pass counts are platform-specific, so each
/// measured host ratchets independently; unmeasured hosts fall back to the
/// global floor. Same rule as minPass: entries should only ever go UP.
struct Manifest {
    /// path -> reason, for DISCRETIONARY skips -- the one denominator lever oam
    /// controls (auto-detected unrunnables are separate and not counted here).
    skips: BTreeMap<String, String>,
    /// subset of `skips` tagged category "known_issues" or "flaky".
    known_issues: usize,
    /// ratchet ceilings; None = not enforced.
    max_skips: Option<usize>,
    max_known_issues: Option<usize>,
    /// pass-count FLOOR: the suite fails if fewer tests pass than this. The
    /// counterpart to the skip ceiling -- it should only ever be RAISED, so a
    /// node-compat regression that drops the pass count reddens CI. None = off.
    min_pass: Option<usize>,
    /// per-host overrides of `min_pass`, keyed by the `{OS}-{ARCH}` host
    /// label (matches the scorecard's `host` field).
    min_pass_by_host: BTreeMap<String, usize>,
}

fn load_manifest(path: &Path) -> Manifest {
    let mut m = Manifest {
        skips: BTreeMap::new(),
        known_issues: 0,
        max_skips: None,
        max_known_issues: None,
        min_pass: None,
        min_pass_by_host: BTreeMap::new(),
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return m;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return m;
    };
    if let Some(r) = v.get("ratchet") {
        m.max_skips = r
            .get("maxSkips")
            .and_then(|x| x.as_u64())
            .map(|x| x as usize);
        m.max_known_issues = r
            .get("maxKnownIssues")
            .and_then(|x| x.as_u64())
            .map(|x| x as usize);
        m.min_pass = r
            .get("minPass")
            .and_then(|x| x.as_u64())
            .map(|x| x as usize);
        if let Some(by_host) = r.get("minPassByHost").and_then(|x| x.as_object()) {
            for (host, floor) in by_host {
                if let Some(floor) = floor.as_u64() {
                    m.min_pass_by_host.insert(host.clone(), floor as usize);
                }
            }
        }
    }
    if let Some(tests) = v.get("tests").and_then(|t| t.as_object()) {
        for (k, cfg) in tests {
            if cfg.get("skip").and_then(|s| s.as_bool()) == Some(true) {
                let reason = cfg
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("manifest skip")
                    .to_string();
                let category = cfg.get("category").and_then(|c| c.as_str()).unwrap_or("");
                if category == "known_issues" || category == "flaky" {
                    m.known_issues += 1;
                }
                m.skips.insert(k.clone(), reason);
            }
        }
    }
    m
}

/// What a test's `// Flags:` header means for runnability.
enum FlagSupport {
    /// No header at all.
    None,
    /// Every flag is one oam implements: pass them through and RUN it.
    Supported(Vec<String>),
    /// At least one flag oam does not implement; carries the raw header.
    Unsupported(String),
}

/// Node flags oam implements (see the node-style parser in oam_cli's main).
/// Deliberately conservative: a flag belongs here only when oam actually
/// honors it, because passing an ignored flag would let a test "pass" while
/// silently not testing what it names.
const SUPPORTED_FLAGS: &[&str] = &[
    "--no-warnings",
    "--no-deprecation",
    "--pending-deprecation",
    "--expose-gc",
    "--redirect-warnings",
    "--disable-warning",
    "--env-file",
    "--env-file-if-exists",
    "--input-type",
    // Forwarded to V8 verbatim.
    "--allow-natives-syntax",
    "--js-float16array",
    "--zero-fill-buffers",
    "--title",
    // Resolves internal/* from the SAME registry the vendored streams port
    // runs on. Tests needing an internal oam does not have reclassify as
    // unrunnable (see missing_builtin), not as failures.
    "--expose-internals",
    "--experimental-vm-modules",
    // Node's permission model: --permission denies everything, the --allow-*
    // flags grant back.
    "--permission",
    "--allow-fs-read",
    "--allow-fs-write",
    "--allow-child-process",
    "--allow-worker",
    "--allow-addons",
];

fn classify_flags(path: &Path) -> FlagSupport {
    let Some(raw) = read_flags_header(path) else {
        return FlagSupport::None;
    };
    let flags: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
    let all_supported = flags
        .iter()
        .all(|f| SUPPORTED_FLAGS.contains(&f.split('=').next().unwrap_or(f)));
    if all_supported && !flags.is_empty() {
        FlagSupport::Supported(flags)
    } else {
        FlagSupport::Unsupported(raw)
    }
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
        "corpus": "test/parallel: core modules (buffer, events, assert, util, querystring, string_decoder, url, path) + I/O-adjacent (stream, process, timers); fs/net/http vendored in later tranches",
        "total": total,
        // pass + fail. Must stay the denominator `passOverRunnable` divides by.
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
    md.push_str("\nCorpus: core modules + the I/O-adjacent `stream` / `process` / `timers` tranche. The big socket/fixture-heavy modules (fs, net, http, child_process, tls) land in later tranches. pass/runnable drops as harder modules are added -- that is the honest denominator widening toward the >85% gate, not a regression; the pass COUNT is floored by ratchet.minPass.\n");
    if !failures.is_empty() {
        md.push_str("\n## Failures (first divergence, triage backlog)\n\n");
        for (name, detail) in failures {
            md.push_str(&format!("- `{name}` -- {detail}\n"));
        }
    }
    std::fs::write(repo.join("CONFORMANCE-NODE.md"), md)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(stderr: &str) -> Captured {
        Captured {
            stdout: String::new(),
            stderr: stderr.to_string(),
            code: 1,
            timed_out: false,
        }
    }

    // -- skip-ratchet: the integrity mechanism against denominator-gaming --

    #[test]
    fn ratchet_within_or_at_ceiling_is_ok() {
        assert!(ratchet_violation(0, 0, 81, Some(0), Some(0), Some(81)).is_none());
        assert!(ratchet_violation(3, 1, 90, Some(5), Some(2), Some(81)).is_none());
        // == ceiling / == floor is allowed; only strictly past them violates.
        assert!(ratchet_violation(5, 2, 81, Some(5), Some(2), Some(81)).is_none());
        // Unset ceilings/floor = unenforced, even with extreme counts.
        assert!(ratchet_violation(99, 99, 0, None, None, None).is_none());
    }

    #[test]
    fn ratchet_skips_over_ceiling_violates() {
        let msg = ratchet_violation(1, 0, 81, Some(0), Some(0), None).expect("must violate");
        assert!(msg.contains("skip-ratchet violation"));
        assert!(msg.contains("1 manifest skips > ceiling 0"));
    }

    #[test]
    fn ratchet_known_issues_over_ceiling_violates() {
        let msg = ratchet_violation(0, 3, 81, Some(10), Some(2), None).expect("must violate");
        assert!(msg.contains("known-issues ceiling violation"));
        assert!(msg.contains("3 known_issues/flaky skips > ceiling 2"));
    }

    #[test]
    fn ratchet_pass_below_floor_violates() {
        // A node-compat regression that drops the pass count must redden CI.
        let msg = ratchet_violation(0, 0, 80, Some(0), Some(0), Some(81)).expect("must violate");
        assert!(msg.contains("pass-floor violation"));
        assert!(msg.contains("80 passing < floor 81"));
    }

    // -- missing_builtin: reclassifies "oam lacks node:X" fail -> unrunnable.
    // Pinned to oam's exact error strings; if the wording drifts, node:test-
    // dependent tests would silently flip unrunnable->fail and tank the number.

    #[test]
    fn missing_builtin_extracts_unknown_node_builtin() {
        let c = cap("error[OAM-MOD0006]: 'node:test' is not a known node: builtin module");
        assert_eq!(missing_builtin(&c).as_deref(), Some("node:test"));
    }

    #[test]
    fn missing_builtin_extracts_cannot_find_module() {
        let c = cap("Error: Cannot find module 'node:inspector/promises' required from x");
        assert_eq!(
            missing_builtin(&c).as_deref(),
            Some("node:inspector/promises")
        );
    }

    #[test]
    fn missing_builtin_none_for_ordinary_assertion_failure() {
        let c = cap("AssertionError: 1 strictEqual 2");
        assert!(missing_builtin(&c).is_none());
    }
}
