//! `cargo run -p xtask -- conformance`: the reliability-with-receipts
//! harness. Three suites, one scorecard:
//!
//! 1. wpt-url — the OFFICIAL web-platform-tests URL data (vendored
//!    snapshot under conformance/vendor/wpt/url) run against oam's URL.
//! 2. node-differential — every conformance/cases/*.mjs executed under
//!    BOTH oam and the installed Node; a case passes when stdout and exit
//!    code are byte-identical.
//! 3. surface — which Node builtin modules load, which globals exist, and
//!    (the gating part) whether every EXPORT NAME Node's builtins carry is
//!    present on oam's. A builtin's ESM named exports are its module
//!    object's own enumerable keys, so a missing name is a link-time
//!    SyntaxError that kills the importing program outright rather than a
//!    lazy failure at the call site — the failure mode that shipped
//!    `import { statfsSync } from "node:fs"` as a crash. Known gaps live in
//!    conformance/surface-gaps.json and may only shrink.
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

    // ------------------------------------------------------ export parity
    // The same runner under NODE, so the oracle is the installed Node on this
    // host rather than a checked-in list: platform-conditional exports (the
    // WSA* errnos on Windows, the SIG* set on POSIX) are then correct by
    // construction instead of needing a manifest per platform.
    let node_surface: Option<Value> = match node.as_deref() {
        Some(node) => {
            let output = run_with_timeout(
                Command::new(node).arg(&surface_runner).current_dir(&repo),
                Duration::from_secs(60),
            )?;
            Some(serde_json::from_str(output.stdout.trim()).with_context(|| {
                format!(
                    "surface runner under node failed; stderr: {}",
                    output.stderr
                )
            })?)
        }
        None => None,
    };
    let gaps = load_surface_gaps(&repo)?;
    // Each section records the Node it was measured against. A different Node
    // here publishes a different set of export names, so gaps can appear or
    // vanish for reasons that have nothing to do with oam -- say so rather
    // than let a version bump read as a regression.
    if let Some(section) = gaps.as_ref()
        && let Some(recorded) = section["generatedAgainst"].as_str()
        && !recorded.starts_with(node_version.trim())
    {
        println!(
            "note: surface-gaps \"{}\" section was recorded against {recorded}, this host runs \
             {node_version}. Export-name differences may be Node's, not oam's.",
            node_platform()
        );
    }
    let exports = compare_exports(gaps.as_ref(), &surface, node_surface.as_ref());

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
        // Hermetic color env: Node honors FORCE_COLOR/COLORTERM even when
        // piped, so a terminal that force-enables color (Yaw sets
        // FORCE_COLOR=3) makes every differential line "diverge" on invisible
        // ANSI escapes. Strip the forcing vars and set NO_COLOR for BOTH
        // sides so the diff never depends on the invoking terminal.
        let oam_out = run_with_timeout(
            Command::new(&oam)
                .arg("run")
                .arg(case)
                .arg("--no-check")
                .env("OAM_CACHE_DIR", &cache)
                .env_remove("FORCE_COLOR")
                .env_remove("COLORTERM")
                .env_remove("CLICOLOR_FORCE")
                .env("NO_COLOR", "1")
                .current_dir(&repo),
            Duration::from_secs(60),
        )?;
        let node_out = run_with_timeout(
            Command::new(node)
                .arg(case)
                .env_remove("FORCE_COLOR")
                .env_remove("COLORTERM")
                .env_remove("CLICOLOR_FORCE")
                .env("NO_COLOR", "1")
                .current_dir(&repo),
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
            // Print the divergence to stdout, not just the scorecard JSON: the
            // scorecard is a CI artifact that isn't surfaced in the run log, so
            // a platform-specific FAIL (e.g. a case that only diverges on Linux)
            // was undiagnosable from `gh run view --log-failed`. Show exit codes,
            // the first differing line (oam vs node), and oam's stderr head.
            if !same_exit {
                println!("    exit: oam={} node={}", oam_out.code, node_out.code);
            }
            if let Some(line) = detail.get("line").and_then(Value::as_u64) {
                let oam_line = detail.get("oam").and_then(Value::as_str).unwrap_or("");
                let node_line = detail.get("node").and_then(Value::as_str).unwrap_or("");
                println!("    first diff at line {line}:");
                println!("      oam:  {oam_line}");
                println!("      node: {node_line}");
            }
            let stderr_head = oam_out
                .stderr
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join(" | ");
            if !stderr_head.is_empty() {
                println!("    oam stderr: {stderr_head}");
            }
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
                "exports": {
                    "skipped": exports.skipped,
                    "platform": node_platform(),
                    "noBaseline": exports.no_baseline,
                    "nodeTotal": exports.node_total,
                    "knownMissing": exports.known_missing,
                    "newMissing": exports.new_missing.iter()
                        .map(|(m, k)| json!({ "module": m, "names": k }))
                        .collect::<Vec<_>>(),
                    "staleRatchet": exports.stale.iter()
                        .map(|(m, k)| json!({ "module": m, "names": k }))
                        .collect::<Vec<_>>(),
                    "absentModules": exports.new_absent_modules.clone(),
                    "staleAbsentModules": exports.stale_absent_modules.clone(),
                    "extra": exports.extra.iter()
                        .map(|(m, k)| json!({ "module": m, "names": k }))
                        .collect::<Vec<_>>(),
                },
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
    md.push_str("Per-name detail lives in the JSON scorecard.\n\n");

    md.push_str("### Builtin export parity\n\n");
    if exports.skipped {
        md.push_str("Skipped — Node absent on this host.\n");
    } else if exports.no_baseline {
        md.push_str(&format!(
            "Measured but **not gated**: `conformance/surface-gaps.json` has no `{}` section. \
             Record one with `node scripts/gen-surface-gaps.mjs` on this host.\n",
            node_platform()
        ));
    } else {
        let implemented = exports.present();
        md.push_str(&format!(
            "**{implemented} / {} Node builtin export names present** ({} known-missing, tracked in \
             [`conformance/surface-gaps.json`](conformance/surface-gaps.json)).\n\n",
            exports.node_total, exports.known_missing
        ));
        md.push_str(
            "A builtin's ESM named exports are its module object's own enumerable keys, so a \
             missing name is a link-time `SyntaxError` that kills the importing program outright \
             — not a lazy failure at the call site. The ratchet gates on NEW gaps and forces the \
             known list to shrink.\n",
        );
    }
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
    if exports.skipped {
        println!("surface-exports: skipped (node absent)");
    } else {
        println!(
            "surface-exports: {}/{} node export names present ({} known-missing, {} new, {} stale)",
            exports.present(),
            exports.node_total,
            exports.known_missing,
            exports
                .new_missing
                .iter()
                .map(|(_, n)| n.len())
                .sum::<usize>()
                + exports.new_absent_modules.len(),
            exports.stale.iter().map(|(_, n)| n.len()).sum::<usize>()
                + exports.stale_absent_modules.len(),
        );
    }
    println!("wrote CONFORMANCE.md + conformance/scorecard.json");

    // Gate: the hand-curated node-differential cases are the deterministic
    // node-compat guard (byte-identical stdout+exit vs Node). Any divergence
    // fails the run (non-zero exit) so the conformance CI job can be REQUIRED,
    // not advisory. The scorecard receipts above are still written first.
    if diff_pass < diff_total {
        bail!(
            "node-differential: {diff_pass}/{diff_total} -- {} case(s) diverge from Node",
            diff_total - diff_pass
        );
    }

    // Gate: builtin export parity. A name Node exports and oam does not is a
    // link-time SyntaxError in any program that imports it, so an UNRECORDED
    // gap fails the run. Recorded gaps (conformance/surface-gaps.json) pass
    // but must shrink: a listed name oam now has fails too, which is what
    // keeps the list from silently covering the next regression.
    if exports.no_baseline && !exports.skipped {
        println!(
            "\nWARNING: conformance/surface-gaps.json has no \"{}\" section, so builtin \
             export parity was MEASURED BUT NOT GATED on this host.\n         \
             {} name(s) and {} module(s) are missing here. Record them with \
             `node scripts/gen-surface-gaps.mjs` and commit the result.",
            node_platform(),
            exports
                .new_missing
                .iter()
                .map(|(_, n)| n.len())
                .sum::<usize>(),
            exports.new_absent_modules.len(),
        );
    }
    if !exports.skipped && !exports.no_baseline {
        let mut problems = Vec::new();
        for (module, names) in &exports.new_missing {
            problems.push(format!(
                "  node:{module} is missing {} export(s) not in surface-gaps.json: {}",
                names.len(),
                names.join(", ")
            ));
        }
        if !exports.new_absent_modules.is_empty() {
            problems.push(format!(
                "  modules absent from oam and not in surface-gaps.json: {}",
                exports.new_absent_modules.join(", ")
            ));
        }
        for (module, names) in &exports.stale {
            problems.push(format!(
                "  node:{module} now HAS {} -- remove from surface-gaps.json (ratchet down)",
                names.join(", ")
            ));
        }
        if !exports.stale_absent_modules.is_empty() {
            problems.push(format!(
                "  modules now present -- remove from surface-gaps.json absentModules: {}",
                exports.stale_absent_modules.join(", ")
            ));
        }
        if !problems.is_empty() {
            bail!(
                "builtin export parity:\n{}\n\nEach missing name is a link-time SyntaxError for \
                 any program that imports it. Implement it, or record it in \
                 conformance/surface-gaps.json with a reason.",
                problems.join("\n")
            );
        }
    }
    Ok(())
}

/// Outcome of diffing oam's per-builtin export names against Node's.
#[derive(Default)]
struct ExportParity {
    /// Node absent -- nothing was compared.
    skipped: bool,
    /// No ratchet section for this platform. The diff below is still real and
    /// worth reporting, but there is no baseline to gate against, so every
    /// gap lands in `new_missing` and the gate must stay quiet.
    no_baseline: bool,
    /// Export names Node has that oam does not, and that the ratchet already
    /// records. Not a failure; the number is the debt still to pay off.
    known_missing: usize,
    /// Export names missing from oam that the ratchet does NOT record. Each
    /// one is a program that dies at link time with "does not provide an
    /// export named X". GATING.
    new_missing: Vec<(String, Vec<String>)>,
    /// Ratchet entries oam now satisfies. GATING, so the list can only
    /// shrink: a closed gap that stays listed lets the next regression hide
    /// behind it.
    stale: Vec<(String, Vec<String>)>,
    /// Modules Node has that oam does not register at all.
    new_absent_modules: Vec<String>,
    stale_absent_modules: Vec<String>,
    /// Names oam exports that Node does not. Advisory: an extra key cannot
    /// break an import, but it is still a fidelity divergence.
    extra: Vec<(String, Vec<String>)>,
    /// Node's total public export names across every compared module.
    node_total: usize,
    /// Node export names belonging to modules oam does not register AT ALL.
    /// Counted separately because the per-name diff never sees them: an
    /// absent module is reported at module granularity and its keys are
    /// skipped, so without this they would silently score as present.
    absent_module_names: usize,
}

impl ExportParity {
    /// Names oam actually has.
    ///
    /// Subtracts all three buckets. `known_missing` + `new_missing` are the
    /// per-name gaps; `absent_module_names` is the 99 names (on
    /// windows-aarch64) living in the 12 modules oam does not register --
    /// `sys` alone is 49 of them. Counting only the first two reported
    /// 1037/1386 where the honest figure is 938/1386, and that number is
    /// published in CONFORMANCE.md.
    fn present(&self) -> usize {
        let missing: usize = self.known_missing
            + self.new_missing.iter().map(|(_, n)| n.len()).sum::<usize>()
            + self.absent_module_names;
        self.node_total.saturating_sub(missing)
    }
}

/// This host's key in surface-gaps.json, spelled the way node's
/// `process.platform` spells it (the generator is JS and writes those keys).
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// Read THIS PLATFORM's section of the checked-in ratchet.
///
/// Sections are per-platform because Node's own builtin surface is: the WSA*
/// errnos exist only on Windows, `process.getuid`/`setgid` only on POSIX. A
/// single flat list measured on one host would report the other hosts'
/// platform-only exports as brand-new gaps and redden a release leg that
/// changed nothing.
///
/// `None` means "no baseline for this platform" -- the caller SKIPS rather
/// than gating, the same as when Node is absent. Failing closed here would
/// block every release on a platform until someone generated a file, and
/// passing silently would claim a verification that never happened.
fn load_surface_gaps(repo: &Path) -> Result<Option<Value>> {
    let path = repo.join("conformance/surface-gaps.json");
    // A MISSING FILE is not a missing baseline -- it is a committed artifact
    // that someone deleted, and treating it as "nothing recorded here yet"
    // would silently turn the gate off on every platform at once. Only the
    // per-platform section may be absent.
    if !path.exists() {
        bail!(
            "{} is missing. It is a committed artifact and the export-parity gate \
             cannot run without it; restore it from git, or regenerate with \
             `node scripts/gen-surface-gaps.mjs`.",
            path.display()
        );
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(doc["platforms"].get(node_platform()).cloned())
}

/// Diff oam's builtin export names against Node's, filtered through the
/// ratchet (conformance/surface-gaps.json, loaded by the caller).
///
/// Pure by design -- no IO -- so the gating decisions are unit-testable the
/// way node_suite::ratchet_violation is. This is the guard for every other
/// export in the runtime; it needs its own tests more than most code does.
fn compare_exports(ratchet: Option<&Value>, oam: &Value, node: Option<&Value>) -> ExportParity {
    let Some(node) = node else {
        return ExportParity {
            skipped: true,
            ..Default::default()
        };
    };
    // No section for this platform: diff against an empty baseline so the
    // numbers are still reported, and flag it so the gate does not fire on
    // gaps nobody has had a chance to record yet.
    let no_baseline = ratchet.is_none();
    let empty_ratchet = json!({ "absentModules": [], "missingExports": {} });
    let ratchet = ratchet.unwrap_or(&empty_ratchet);

    let allowed_absent: Vec<&str> = ratchet["absentModules"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let empty = serde_json::Map::new();
    let node_map = node["exportNames"].as_object().unwrap_or(&empty);
    let oam_map = oam["exportNames"].as_object().unwrap_or(&empty);

    let mut out = ExportParity {
        no_baseline,
        ..Default::default()
    };
    let mut seen_absent = Vec::new();

    for (module, node_entry) in node_map {
        // Only Node's own answer defines the target. A module Node does not
        // have (or whose exports are a primitive) has no named exports to
        // miss.
        let Some(node_keys) = node_entry.as_array() else {
            continue;
        };
        let node_keys: Vec<&str> = node_keys.iter().filter_map(Value::as_str).collect();
        out.node_total += node_keys.len();

        let oam_entry = oam_map.get(module).unwrap_or(&Value::Bool(false));
        // Module absent in oam entirely: report at module granularity rather
        // than spraying every one of its export names into the diff.
        if oam_entry.as_bool() == Some(false) || oam_entry.is_null() {
            seen_absent.push(module.clone());
            out.absent_module_names += node_keys.len();
            if !allowed_absent.contains(&module.as_str()) {
                out.new_absent_modules.push(module.clone());
            }
            continue;
        }
        let oam_keys: Vec<&str> = oam_entry
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let listed: Vec<&str> = ratchet["missingExports"][module]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let mut new_missing = Vec::new();
        for key in &node_keys {
            if oam_keys.contains(key) {
                continue;
            }
            if listed.contains(key) {
                out.known_missing += 1;
            } else {
                new_missing.push((*key).to_string());
            }
        }
        if !new_missing.is_empty() {
            out.new_missing.push((module.clone(), new_missing));
        }

        // Ratchet-down: a listed name oam now HAS must leave the list.
        let stale: Vec<String> = listed
            .iter()
            .filter(|key| oam_keys.contains(key))
            .map(|key| (*key).to_string())
            .collect();
        if !stale.is_empty() {
            out.stale.push((module.clone(), stale));
        }

        let extra: Vec<String> = oam_keys
            .iter()
            .filter(|key| !node_keys.contains(key))
            .map(|key| (*key).to_string())
            .collect();
        if !extra.is_empty() {
            out.extra.push((module.clone(), extra));
        }
    }

    // Ratchet-down for modules too.
    out.stale_absent_modules = allowed_absent
        .iter()
        .filter(|m| !seen_absent.contains(&(**m).to_string()))
        // Only claim staleness for modules Node actually has; a name Node
        // dropped in a later release is not oam's to implement.
        .filter(|m| node_map.get(**m).and_then(Value::as_array).is_some())
        .map(|m| (*m).to_string())
        .collect();

    out
}

pub(crate) fn repo_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest
        .parent()
        .context("xtask manifest has a parent")?
        .to_path_buf())
}

pub(crate) fn ensure_oam_built(repo: &Path, release: bool) -> Result<PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let exe = repo
        .join(format!("target/{profile}"))
        .join(format!("oam{}", std::env::consts::EXE_SUFFIX));
    // ALWAYS build, never gate on the binary existing: cargo's own freshness
    // check makes this a ~1s no-op when current, and an existing-but-STALE
    // binary silently measures month-old code. Bit for real on 2026-08-04:
    // the mac measure leg preserved target/ across source syncs, xtask saw
    // target/debug/oam from July 22 and ran the whole node-suite against it
    // -- 140 phantom failures against today's corpus.
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

pub(crate) struct Captured {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) code: i32,
    pub(crate) timed_out: bool,
}

/// Spawn with piped output and a hard deadline (the oam_mcp try_wait
/// pattern): a hung case must fail the suite, not the harness.
pub(crate) fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Captured> {
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

pub(crate) fn normalize(text: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A surface-runner document with one module's export names.
    fn surface(entries: &[(&str, Option<&[&str]>)]) -> Value {
        let mut map = serde_json::Map::new();
        for (name, keys) in entries {
            map.insert(
                (*name).to_string(),
                match keys {
                    // None models "module absent" -- what the runner emits
                    // when require() throws.
                    None => json!(false),
                    Some(keys) => json!(keys),
                },
            );
        }
        json!({ "exportNames": map })
    }

    fn ratchet(absent: &[&str], missing: &[(&str, &[&str])]) -> Value {
        let mut map = serde_json::Map::new();
        for (module, names) in missing {
            map.insert((*module).to_string(), json!(names));
        }
        json!({ "absentModules": absent, "missingExports": map })
    }

    // -- new gaps: the case that shipped `import { statfsSync }` as a crash --

    #[test]
    fn unrecorded_missing_export_is_a_new_gap() {
        let out = compare_exports(
            Some(&ratchet(&[], &[])),
            &surface(&[("fs", Some(&["statSync"]))]),
            Some(&surface(&[("fs", Some(&["statSync", "statfsSync"]))])),
        );
        assert_eq!(
            out.new_missing,
            vec![("fs".to_string(), vec!["statfsSync".to_string()])]
        );
        assert_eq!(out.known_missing, 0);
    }

    #[test]
    fn recorded_missing_export_is_known_debt_not_a_failure() {
        let out = compare_exports(
            Some(&ratchet(&[], &[("fs", &["statfsSync"])])),
            &surface(&[("fs", Some(&["statSync"]))]),
            Some(&surface(&[("fs", Some(&["statSync", "statfsSync"]))])),
        );
        assert!(out.new_missing.is_empty());
        assert_eq!(out.known_missing, 1);
        assert_eq!(out.node_total, 2);
    }

    // -- ratchet-down: a closed gap must LEAVE the list, or it becomes cover
    //    for the next regression in the same module.

    #[test]
    fn implemented_export_still_listed_is_stale() {
        let out = compare_exports(
            Some(&ratchet(&[], &[("fs", &["statfsSync"])])),
            &surface(&[("fs", Some(&["statSync", "statfsSync"]))]),
            Some(&surface(&[("fs", Some(&["statSync", "statfsSync"]))])),
        );
        assert_eq!(
            out.stale,
            vec![("fs".to_string(), vec!["statfsSync".to_string()])]
        );
    }

    // -- absent modules --

    #[test]
    fn unrecorded_absent_module_is_reported() {
        let out = compare_exports(
            Some(&ratchet(&[], &[])),
            &surface(&[("wasi", None)]),
            Some(&surface(&[("wasi", Some(&["WASI"]))])),
        );
        assert_eq!(out.new_absent_modules, vec!["wasi".to_string()]);
        assert!(out.stale_absent_modules.is_empty());
    }

    #[test]
    fn registered_module_still_in_absent_list_is_stale() {
        let out = compare_exports(
            Some(&ratchet(&["wasi"], &[])),
            &surface(&[("wasi", Some(&["WASI"]))]),
            Some(&surface(&[("wasi", Some(&["WASI"]))])),
        );
        assert_eq!(out.stale_absent_modules, vec!["wasi".to_string()]);
        assert!(out.new_absent_modules.is_empty());
    }

    /// A module Node itself dropped must NOT be reported stale: oam removing
    /// a shim for something Node no longer ships is not a ratchet violation,
    /// and demanding the entry's removal would wedge the gate on a change oam
    /// did not make.
    #[test]
    fn absent_entry_for_a_module_node_lacks_is_not_stale() {
        let out = compare_exports(
            Some(&ratchet(&["sys"], &[])),
            &surface(&[("fs", Some(&["statSync"]))]),
            Some(&surface(&[("fs", Some(&["statSync"]))])),
        );
        assert!(out.stale_absent_modules.is_empty());
        assert!(out.new_absent_modules.is_empty());
    }

    // -- oracle absent --

    /// Without Node there is no oracle. The run must report SKIPPED rather
    /// than an empty (all-clear) diff, which would read as "parity verified"
    /// on a host that verified nothing.
    #[test]
    fn no_node_skips_instead_of_passing_vacuously() {
        let out = compare_exports(
            Some(&ratchet(&[], &[])),
            &surface(&[("fs", Some(&["x"]))]),
            None,
        );
        assert!(out.skipped);
        assert_eq!(out.node_total, 0);
        assert!(out.new_missing.is_empty());
    }

    /// An absent module's exports must count as MISSING, not present. They
    /// never reach the per-name diff (the module is reported whole), so
    /// `present()` has to subtract them explicitly -- without that, the 12
    /// unregistered modules' 99 names scored as implemented and the published
    /// figure read 1037/1386 instead of 938/1386.
    #[test]
    fn absent_module_exports_do_not_count_as_present() {
        let out = compare_exports(
            Some(&ratchet(&["wasi"], &[])),
            &surface(&[("fs", Some(&["statSync"])), ("wasi", None)]),
            Some(&surface(&[
                ("fs", Some(&["statSync"])),
                ("wasi", Some(&["WASI", "WASIServe"])),
            ])),
        );
        assert_eq!(out.node_total, 3);
        assert_eq!(out.absent_module_names, 2);
        // Only node:fs's one name is genuinely there.
        assert_eq!(out.present(), 1);
    }

    // -- no baseline for this platform --

    /// A platform with no ratchet section still gets MEASURED, but must not
    /// gate: every gap would land in `new_missing` and redden a release leg
    /// on a host nobody has recorded a baseline for yet. Failing closed there
    /// blocks the release on a missing file; passing silently would claim a
    /// verification that never ran. The caller keys off `no_baseline`.
    #[test]
    fn missing_platform_section_measures_without_gating() {
        let out = compare_exports(
            None,
            &surface(&[("fs", Some(&["statSync"]))]),
            Some(&surface(&[("fs", Some(&["statSync", "statfsSync"]))])),
        );
        assert!(out.no_baseline);
        assert!(!out.skipped);
        // The diff is real -- the numbers are still reportable.
        assert_eq!(
            out.new_missing,
            vec![("fs".to_string(), vec!["statfsSync".to_string()])]
        );
    }

    #[test]
    fn present_platform_section_gates_normally() {
        let out = compare_exports(
            Some(&ratchet(&[], &[("fs", &["statfsSync"])])),
            &surface(&[("fs", Some(&["statSync"]))]),
            Some(&surface(&[("fs", Some(&["statSync", "statfsSync"]))])),
        );
        assert!(!out.no_baseline);
        assert!(out.new_missing.is_empty());
    }

    // -- extras are advisory, never gating --

    #[test]
    fn export_oam_has_and_node_lacks_is_extra_not_missing() {
        let out = compare_exports(
            Some(&ratchet(&[], &[])),
            &surface(&[("oamx", Some(&["a", "b"]))]),
            Some(&surface(&[("oamx", Some(&["a"]))])),
        );
        assert!(out.new_missing.is_empty());
        assert_eq!(out.extra, vec![("oamx".to_string(), vec!["b".to_string()])]);
    }
}
