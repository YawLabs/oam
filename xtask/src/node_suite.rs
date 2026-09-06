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
use std::collections::{BTreeMap, BTreeSet};
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

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    /// The test self-skipped: it printed the TAP `1..0 # Skipped` marker AND
    /// exited 0. Carries the reason `common.skip()` already puts on the wire
    /// (common/index.js:548) so the committed receipt can NAME every test that
    /// left the denominator instead of publishing a bare per-module integer.
    Skip(String),
    Fail(String),
    Unrunnable(String, UnrunnableKind),
}

/// Why a test never produced a verdict. Published beside the reason so a
/// reader can tell an oam-side gap (a module oam does not implement) from a
/// harness/vendoring one (an explicit manifest entry, an unsupported flag)
/// without re-deriving it from prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnrunnableKind {
    /// An explicit `"skip": true` entry in manifest.json.
    Manifest,
    /// A `// Flags:` header naming at least one flag oam does not implement.
    Flags,
    /// The test imports a `node:`/`internal/` module -- or an internalBinding
    /// namespace -- that oam does not provide.
    MissingModule,
}

impl UnrunnableKind {
    fn as_str(self) -> &'static str {
        match self {
            UnrunnableKind::Manifest => "manifest",
            UnrunnableKind::Flags => "flags",
            UnrunnableKind::MissingModule => "missing-module",
        }
    }
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

    let manifest = load_manifest(&vendor.join("manifest.json"))?;

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

    // Pin the denominator. The corpus is enumerated with read_dir, so a file
    // silently dropped from (or added to) the vendored tree moves the
    // population every published rate divides by, with nothing to notice --
    // the ratchets floor the PASS count, not the corpus it is drawn from.
    // `expectedTotal` makes that population a committed, reviewable number.
    // Checked BEFORE the run so a drifted corpus fails in a second rather than
    // after 476 subprocesses. Offline by construction: no git, no network.
    if let Some(expected) = manifest.expected_total
        && tests.len() != expected
    {
        bail!("{}", corpus_drift_message(&tests, expected, &manifest));
    }

    // per-module tallies: [pass, fail, skip, unrunnable]
    let mut by_module: BTreeMap<String, [usize; 4]> = BTreeMap::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    // (name, first divergence, why it is intentional) -- see the Fail arm below.
    let mut deliberate_failures: Vec<(String, String, String)> = Vec::new();
    // The two exclusion lists, NAMED. Per-module integers told a reader that 22
    // tests left the denominator but not WHICH, so a compensating change that
    // skipped one test and un-skipped another was invisible to every number in
    // the receipt. These carry the reason (and, for unrunnables, the kind) into
    // both committed receipts.
    let mut skips: Vec<(String, String)> = Vec::new();
    let mut unrunnables: Vec<(String, String, UnrunnableKind)> = Vec::new();
    // manifest key -> did the run actually see the TAP skip marker the
    // partialSkip entry claims is there? An entry that no longer matches is
    // reported STALE below, the same as a deliberate entry that stopped failing.
    let mut partial_skip_marker_seen: BTreeSet<String> = BTreeSet::new();
    let (mut pass, mut fail) = (0usize, 0usize);

    for test in &tests {
        let name = test
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let module = module_of(&name);
        let key = format!("parallel/{name}");
        let partial_skip = manifest.partial_skips.contains_key(&key);

        let outcome = if let Some(reason) = manifest.skips.get(&key) {
            Outcome::Unrunnable(reason.clone(), UnrunnableKind::Manifest)
        } else {
            // A `// Flags:` header is only disqualifying when oam cannot
            // honor the flags. Excluding EVERY flagged test shrank the
            // denominator with tests oam runs correctly -- a measurement
            // that flatters the runtime for free. Pass through what we
            // support; exclude only the genuinely unsupported.
            match classify_flags(test) {
                FlagSupport::Unsupported(raw) => {
                    Outcome::Unrunnable(format!("// Flags:{raw}"), UnrunnableKind::Flags)
                }
                // `None` and `Supported` differ only in the flag vector;
                // classify_flags never returns an empty `Supported`, so the
                // empty slice still selects run_test's `oam run <file>` form.
                supported => {
                    let flags = match supported {
                        FlagSupport::Supported(flags) => flags,
                        _ => Vec::new(),
                    };
                    let result = run_test(&oam, test, &vendor, &cache, &flags, partial_skip);
                    if partial_skip && result.saw_skip_marker {
                        partial_skip_marker_seen.insert(key.clone());
                    }
                    result.outcome
                }
            }
        };

        let slot = by_module.entry(module).or_insert([0; 4]);
        match outcome {
            Outcome::Pass => {
                pass += 1;
                slot[0] += 1;
                println!("  PASS   {name}");
            }
            Outcome::Skip(reason) => {
                slot[2] += 1;
                println!("  SKIP   {name}  ({reason})");
                skips.push((name.clone(), reason));
            }
            Outcome::Fail(detail) => {
                fail += 1;
                slot[1] += 1;
                // A DELIBERATE divergence is still a failure in every number --
                // it fails, it counts, it stays in the denominator. The manifest
                // entry only records WHY, so the report stops filing a settled
                // decision under "triage backlog" and the next reader does not
                // re-litigate it. Reclassifying is ratcheted (maxDeliberate) so
                // a real bug cannot be quietly relabelled as intentional.
                let deliberate = manifest.deliberate.get(&format!("parallel/{name}"));
                println!(
                    "  {}   {name}  {detail}",
                    if deliberate.is_some() {
                        "XFAIL"
                    } else {
                        "FAIL "
                    }
                );
                match deliberate {
                    Some(reason) => {
                        deliberate_failures.push((name.clone(), detail, reason.clone()))
                    }
                    None => failures.push((name.clone(), detail)),
                }
            }
            Outcome::Unrunnable(reason, kind) => {
                slot[3] += 1;
                println!("  UNRUN  {name}  [{}] ({reason})", kind.as_str());
                unrunnables.push((name.clone(), reason, kind));
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

    // The partial-skip lever, NAMED in the receipt for the same reason the two
    // exclusion lists are: it moves a test INTO the scored denominator over its
    // own TAP marker, so a reader must be able to see which file and why
    // without re-deriving it from manifest.json.
    let partial_skip_entries: Vec<(String, String)> = manifest
        .partial_skips
        .iter()
        .map(|(key, reason)| {
            (
                key.rsplit('/').next().unwrap_or(key).to_string(),
                reason.clone(),
            )
        })
        .collect();

    let card = Scorecard {
        by_module: &by_module,
        failures: &failures,
        deliberate: &deliberate_failures,
        skips: &skips,
        partial_skips: &partial_skip_entries,
        unrunnable: &unrunnables,
        total,
        pass,
        fail,
    };
    let rewrote = write_scorecard(&repo, &oam, &card)?;

    println!();
    println!("node-suite (vendored Node v22.22.2 subset):");
    println!(
        "  pass/runnable = {pass}/{scored} ({:.1}%)   pass/total = {pass}/{total} ({:.1}%)",
        pct(pass, scored),
        pct(pass, total)
    );
    println!(
        "  {pass} pass  {fail} fail  {} skip  {} unrunnable-by-harness",
        skips.len(),
        unrunnables.len()
    );
    println!(
        "{}",
        if rewrote {
            "wrote CONFORMANCE-NODE.md + conformance/node-suite-scorecard.json (results changed)"
        } else {
            "CONFORMANCE-NODE.md + conformance/node-suite-scorecard.json unchanged (tree left clean)"
        }
    );

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
    if !manifest.deliberate.is_empty() {
        println!(
            "deliberate divergences: {}/{} (ratchet ceiling) -- counted as failures, still in the denominator",
            deliberate_failures.len(),
            manifest
                .max_deliberate
                .map(|m| m.to_string())
                .unwrap_or_else(|| "-".into())
        );
    }
    // A manifest entry that no longer fails is stale: the divergence was fixed
    // (or the test moved) and the annotation now describes nothing. Say so
    // rather than let it rot into a claim about a test that passes.
    for name in manifest.deliberate.keys() {
        let still_failing = deliberate_failures
            .iter()
            .any(|(n, _, _)| format!("parallel/{n}") == *name);
        if !still_failing {
            println!("  STALE  {name} is marked deliberate but did not fail -- drop the entry");
        }
    }
    if !manifest.partial_skips.is_empty() {
        println!(
            "partial skips: {}/{} (ratchet ceiling) -- marker ignored, scored by exit code, IN the denominator",
            manifest.partial_skips.len(),
            manifest
                .max_partial_skips
                .map(|m| m.to_string())
                .unwrap_or_else(|| "-".into())
        );
    }
    // Same staleness rule as `deliberate` above: an entry that no longer
    // describes the test it names is a claim about nothing. A partialSkip
    // overrides a marker, so an entry whose test prints no marker (it was
    // fixed, it stopped feature-detecting, it never ran) is overriding nothing
    // and should be dropped rather than left to rot.
    for name in manifest.partial_skips.keys() {
        if !partial_skip_marker_seen.contains(name) {
            println!(
                "  STALE  {name} is marked partialSkip but printed no TAP skip marker -- drop the entry"
            );
        }
    }
    if let Some(msg) = ratchet_violation(&Ratchet {
        skips: manifest_skips,
        known_issues: manifest.known_issues,
        deliberate: deliberate_failures.len(),
        partial_skips: manifest.partial_skips.len(),
        pass,
        max_skips: manifest.max_skips,
        max_known_issues: manifest.max_known_issues,
        max_deliberate: manifest.max_deliberate,
        max_partial_skips: manifest.max_partial_skips,
        min_pass,
    }) {
        bail!("{msg}");
    }
    Ok(())
}

/// What one run measured, paired with the committed limits it must respect.
///
/// Was eight positional arguments behind an `#[allow(too_many_arguments)]`, on
/// the reasoning that a struct would only rename the same values. Adding the
/// partial-skip lever takes it to ten -- five bare `usize` counts followed by
/// five bare `Option<usize>` limits, every one of them transposable at a call
/// site with nothing to catch it, in the function whose whole job is to be the
/// integrity check. Named fields cost the tests one `..Ratchet` literal each
/// and make a transposition a compile error.
#[derive(Default)]
struct Ratchet {
    /// discretionary manifest skips (auto-detected unrunnables excluded).
    skips: usize,
    known_issues: usize,
    deliberate: usize,
    partial_skips: usize,
    pass: usize,
    /// ceilings and floors; None = not enforced.
    max_skips: Option<usize>,
    max_known_issues: Option<usize>,
    max_deliberate: Option<usize>,
    max_partial_skips: Option<usize>,
    min_pass: Option<usize>,
}

/// Pure ratchet check (extracted for unit testing): returns Some(message) when
/// the discretionary skip count, the known-issues count, the
/// deliberate-divergence count or the partial-skip count exceeds its ceiling,
/// OR the pass count falls below its floor. None = all within limits / unset.
/// `== ceiling` and `== floor` are allowed; only strictly past them violates.
fn ratchet_violation(r: &Ratchet) -> Option<String> {
    if let Some(max) = r.max_skips
        && r.skips > max
    {
        return Some(format!(
            "skip-ratchet violation: {} manifest skips > ceiling {max}. \
             Fix the tests, or (with review) raise ratchet.maxSkips in manifest.json -- it should only ever go DOWN.",
            r.skips
        ));
    }
    if let Some(max) = r.max_known_issues
        && r.known_issues > max
    {
        return Some(format!(
            "known-issues ceiling violation: {} known_issues/flaky skips > ceiling {max}.",
            r.known_issues
        ));
    }
    if let Some(max) = r.max_deliberate
        && r.deliberate > max
    {
        return Some(format!(
            "deliberate-divergence ceiling violation: {} > ceiling {max}. A test that fails on \
             purpose needs an explicit ratchet bump, so a real regression cannot be relabelled \
             as intentional.",
            r.deliberate
        ));
    }
    if let Some(max) = r.max_partial_skips
        && r.partial_skips > max
    {
        return Some(format!(
            "partial-skip ceiling violation: {} > ceiling {max}. A partialSkip entry overrides a \
             test's own TAP skip marker and scores it by exit code, which puts it back INTO the \
             denominator -- so each one is a reviewed claim about a specific file, not a rule. \
             Raising ratchet.maxPartialSkips needs that review; it should only ever go DOWN.",
            r.partial_skips
        ));
    }
    if let Some(min) = r.min_pass
        && r.pass < min
    {
        return Some(format!(
            "pass-floor violation: {} passing < floor {min}. A node-compat regression \
             dropped the pass count. Fix it, or (with review) lower ratchet.minPass -- it should only ever go UP.",
            r.pass
        ));
    }
    None
}

/// One test's verdict, plus the one fact about HOW it got there that the
/// caller cannot re-derive from the outcome: whether the winning attempt
/// printed a TAP skip marker. Read only for `partialSkip` manifest entries --
/// an entry naming a test that no longer prints one describes nothing, and is
/// reported STALE the same way a no-longer-failing `deliberate` entry is.
struct TestResult {
    outcome: Outcome,
    saw_skip_marker: bool,
}

/// Run one test, with a 3x flaky rerun: a Pass on any attempt wins; otherwise
/// the last Fail/Skip stands.
///
/// `partial_skip` comes from the manifest and means "this test's TAP skip
/// marker does not describe how its run ended" -- see `classify_attempt`.
///
/// `cwd` is the VENDOR root, not the oam repo: Node runs its suite from the
/// Node repo root, and the vendored tree is our stand-in for it (same
/// `test/`, `lib/`, package.json layout). A handful of tests read
/// `process.cwd()` directly.
fn run_test(
    oam: &Path,
    test: &Path,
    cwd: &Path,
    cache: &Path,
    flags: &[String],
    partial_skip: bool,
) -> TestResult {
    let mut last = TestResult {
        outcome: Outcome::Fail("no attempt".to_string()),
        saw_skip_marker: false,
    };
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
            Err(e) => {
                return TestResult {
                    outcome: Outcome::Fail(format!("harness error: {e}")),
                    saw_skip_marker: false,
                };
            }
        };

        let saw_skip_marker = skip_marker_reason(&out.stdout).is_some();
        // Only a Fail is worth re-running; everything else is settled.
        match classify_attempt(&out, partial_skip) {
            Outcome::Fail(detail) => {
                last = TestResult {
                    outcome: Outcome::Fail(detail),
                    saw_skip_marker,
                };
            }
            settled => {
                return TestResult {
                    outcome: settled,
                    saw_skip_marker,
                };
            }
        }
    }
    last
}

/// Score ONE finished attempt. Pure over the captured output, so the ordering
/// rule below is unit-testable without spawning anything.
///
/// common.skip() prints `1..0 # Skipped` and THEN exits. The marker alone is
/// NOT authoritative -- common.printSkipMessage() prints the same line without
/// exiting -- so how the process ENDED decides first, and the marker only
/// picks Skip-vs-Pass on a process that actually exited 0:
///
/// * marker + exit 0    -> Skip (keep the reason it put on the wire)
/// * marker + exit != 0 -> Fail (the run did NOT end on the skip)
/// * marker + timeout   -> Fail (ditto, and it hung)
///
/// The last two used to be scored Skip, i.e. silently dropped out of the
/// denominator: a test that printed a skip marker and then CRASHED, or hung
/// until the 150s deadline, left no trace in any published number.
///
/// `partial_skip` is the one case where the marker is NOT consulted at all: a
/// PARTIAL skip that then finishes. test-buffer-alloc.js calls
/// printSkipMessage() at :1076, runs its remaining ~120 lines of Buffer
/// assertions, and exits 0 -- and its stdout is then byte-identical to
/// test-stream-pipeline-http2.js, which common.skip()s at the top and asserts
/// nothing: one `1..0 # Skipped: missing crypto` line, exit 0. (Both because
/// `common.hasCrypto` is `Boolean(process.versions.openssl)`,
/// common/index.js:54, and oam links ring and rustls rather than OpenSSL.) No
/// output-only rule separates them, and scoring exit-0-with-marker as a Pass in
/// general would count the second test -- which asserted NOTHING -- as a pass,
/// inflating the very number minPass floors.
///
/// So the default is unchanged (marker + exit 0 = Skip), and the separation is
/// made by a human instead: a `"partialSkip": true` entry in manifest.json
/// names ONE file as a test that really does assert past its marker, and for
/// that file the exit code decides as if no marker had been printed. It is a
/// reviewable diff, ratcheted by maxPartialSkips, and it names the test in both
/// receipts -- not a rule that could mint a false PASS on the next test that
/// happens to skip transitively in some helper's module body.
///
/// Carve-out to re-check if the corpus ever widens: common.skip() exits 1 for
/// tests under test/known_issues/, where marker + non-zero exit IS a
/// legitimate skip. Only test/parallel is vendored (see the parallel_dir bail
/// in run()), so today that combination is unambiguously a bug -- vendoring a
/// known_issues tranche would need a path guard here.
fn classify_attempt(out: &Captured, partial_skip: bool) -> Outcome {
    if out.timed_out {
        return Outcome::Fail("[TIMEOUT]".to_string());
    }
    if out.code == 0 {
        return match skip_marker_reason(&out.stdout) {
            Some(reason) if !partial_skip => Outcome::Skip(reason),
            _ => Outcome::Pass,
        };
    }
    if let Some(builtin) = missing_builtin(out) {
        return Outcome::Unrunnable(
            format!("needs unimplemented {builtin}"),
            UnrunnableKind::MissingModule,
        );
    }
    Outcome::Fail(format!("[exit={}] {}", out.code, first_error_line(out)))
}

/// The reason from a TAP skip marker, if the test printed one.
///
/// `common.printSkipMessage` emits `1..0 # Skipped: <msg>` (common/index.js:548)
/// and the harness used to throw the `<msg>` away, leaving the receipt able to
/// say only how MANY tests self-skipped. Some tests hand-roll the bare
/// `1..0 # SKIP` spelling with no reason at all -- hence the fallback text
/// rather than an Option the callers would have to re-describe.
fn skip_marker_reason(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        // Longest first: "1..0 # SKIP" is a prefix of "1..0 # Skipped".
        for marker in ["1..0 # Skipped", "1..0 # SKIP"] {
            if let Some(idx) = line.find(marker) {
                let rest = line[idx + marker.len()..].trim_start();
                let reason = rest.strip_prefix(':').unwrap_or(rest).trim();
                return Some(if reason.is_empty() {
                    "no reason given".to_string()
                } else {
                    reason.to_string()
                });
            }
        }
    }
    None
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
    // Same class again, one layer down: `internalBinding('ns').member` where
    // oam has nothing real behind `ns` (or behind that member of it). The
    // throwing proxy in js/node_compat.js reports it as
    //   oam: no native binding for 'ns.member'
    // -- a deliberately distinctive prefix, and the ONLY string matched here.
    // A bare "No such module" would also swallow genuine failures, and an
    // unbacked binding that returned `{}` instead of throwing would be worse
    // still: the test would run on against a surface oam does not have.
    no_binding_name(&hay)
}

/// The prefix the throwing internalBinding proxy (js/node_compat.js) puts on
/// the wire. Deliberately distinctive so nothing else can match it.
const NO_BINDING: &str = "oam: no native binding for '";

/// The binding name out of a THROWN `oam: no native binding for '<ns>.<member>'`
/// message -- read from the rendered error line, never from an echoed source
/// line.
///
/// A bare substring search over the captured output does not work here, because
/// oam's fatal report prints a CODE FRAME first: the source line of the throw
/// site, verbatim, template literal and all.
///
///     oam:node_compat.js:22667
///             throw new Error(`oam: no native binding for '${id}'`);
///             ^
///
///     Error: oam: no native binding for 'js_stream'
///
/// So the FIRST occurrence in the stream is the uninterpolated template, and
/// the committed receipt published `needs unimplemented ${id}` /
/// `needs unimplemented ${ns}.${String(prop)}` for four tests. It was only ever
/// four because the bug is occurrence-order dependent: a throw site whose frame
/// oam does not echo (test-buffer-fill, where the binding error is nested inside
/// an AssertionError's inspected `actual:`) extracted the real name.
///
/// The anchor is therefore the error RENDERING immediately before the message --
/// `Error: `, `TypeError: `, `AssertionError: `, and the `actual: Error: ` form
/// util.inspect uses for a nested throw -- which the echoed `throw new Error(`
/// source line can never match. A candidate still carrying `${` is rejected
/// outright as a second, independent guard.
///
/// Classification does NOT depend on the parse succeeding: if the prefix is
/// present but no line renders it as a message we can name, the test is still
/// unrunnable, just unnameable. Anything else would let a truncated pipe turn an
/// oam gap into a correctness failure.
fn no_binding_name(hay: &str) -> Option<String> {
    let mut seen_anywhere = false;
    for line in hay.lines() {
        let line = line.trim();
        let Some(idx) = line.find(NO_BINDING) else {
            continue;
        };
        seen_anywhere = true;
        if !line[..idx].ends_with("Error: ") {
            continue;
        }
        let rest = &line[idx + NO_BINDING.len()..];
        let Some(end) = rest.find('\'') else {
            continue;
        };
        let name = &rest[..end];
        if name.is_empty() || name.contains("${") {
            continue;
        }
        return Some(name.to_string());
    }
    seen_anywhere.then(|| "an internalBinding namespace".to_string())
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
///   "ratchet": { "maxSkips": N, "maxKnownIssues": M, "maxDeliberate": D,
///                "maxPartialSkips": P, "minPass": K,
///                "minPassByHost": { "<os>-<arch>": K2, ... } },
///   "tests": { "parallel/test-x.js": { "skip": true, "reason": "...", "category": "known_issues" } }
/// }
///
/// A `tests` entry carries exactly ONE flavour -- `"skip"`, `"deliberate"` or
/// `"partialSkip"` -- each with its own ceiling; two on one entry is a hard
/// error, not a precedence rule (see load_manifest).
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
    /// path -> why the divergence is INTENTIONAL, for tests oam fails on
    /// purpose and will keep failing. Purely a reporting classification: the
    /// test still runs, still counts as a failure, and stays in the
    /// denominator, so this cannot be used to flatter the pass rate the way a
    /// skip could. Ratcheted by `maxDeliberate` all the same.
    deliberate: BTreeMap<String, String>,
    /// path -> why the test's TAP skip marker does NOT describe how its run
    /// ended. `common.printSkipMessage()` prints `1..0 # Skipped: <msg>`
    /// WITHOUT exiting, so a test can skip one section part-way through and
    /// then assert its way to the end of the file (test-buffer-alloc.js:1076,
    /// then ~120 more lines of Buffer assertions, then exit 0). Its output is
    /// byte-identical to a test that `common.skip()`s at the top and asserts
    /// NOTHING, so no output-only rule can separate them -- see
    /// `classify_attempt`. This is the explicit, reviewable opt-in instead: for
    /// a named test the marker is ignored and the exit code decides, which puts
    /// it back in pass/fail. Ratcheted by `maxPartialSkips` so it cannot grow
    /// into a general "score it as a pass" escape hatch.
    partial_skips: BTreeMap<String, String>,
    /// ratchet ceilings; None = not enforced.
    max_skips: Option<usize>,
    max_known_issues: Option<usize>,
    max_deliberate: Option<usize>,
    max_partial_skips: Option<usize>,
    /// pass-count FLOOR: the suite fails if fewer tests pass than this. The
    /// counterpart to the skip ceiling -- it should only ever be RAISED, so a
    /// node-compat regression that drops the pass count reddens CI. None = off.
    min_pass: Option<usize>,
    /// per-host overrides of `min_pass`, keyed by the `{OS}-{ARCH}` host
    /// label (matches the scorecard's `host` field).
    min_pass_by_host: BTreeMap<String, usize>,
    /// how many test files the vendored corpus is expected to contain. The
    /// corpus is enumerated with read_dir, so without this the population every
    /// rate divides by is whatever happens to be on disk. None = not pinned.
    expected_total: Option<usize>,
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let mut m = Manifest {
        skips: BTreeMap::new(),
        known_issues: 0,
        deliberate: BTreeMap::new(),
        partial_skips: BTreeMap::new(),
        max_skips: None,
        max_known_issues: None,
        max_deliberate: None,
        max_partial_skips: None,
        min_pass: None,
        min_pass_by_host: BTreeMap::new(),
        expected_total: None,
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(m);
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Ok(m);
    };
    // Top level, next to nodeVersion -- it describes the vendored CORPUS, not a
    // ratchet on oam's behavior. Accepted under "ratchet" too so a reader who
    // files it with the other numbers still gets the check rather than silence.
    m.expected_total = v
        .get("expectedTotal")
        .or_else(|| v.get("ratchet").and_then(|r| r.get("expectedTotal")))
        .and_then(|x| x.as_u64())
        .map(|x| x as usize);
    if let Some(r) = v.get("ratchet") {
        m.max_skips = r
            .get("maxSkips")
            .and_then(|x| x.as_u64())
            .map(|x| x as usize);
        m.max_known_issues = r
            .get("maxKnownIssues")
            .and_then(|x| x.as_u64())
            .map(|x| x as usize);
        m.max_deliberate = r
            .get("maxDeliberate")
            .and_then(|x| x.as_u64())
            .map(|x| x as usize);
        // Deliberately NOT maxSkips: that ceiling is 0 and means "no test may
        // leave the denominator by manifest fiat". A partialSkip does the
        // opposite -- it puts one back IN -- so folding the two into one number
        // would let a skip be traded for a partial skip with nothing to review.
        m.max_partial_skips = r
            .get("maxPartialSkips")
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
            let flag = |name: &str| cfg.get(name).and_then(|b| b.as_bool()) == Some(true);
            let (skip, deliberate, partial) =
                (flag("skip"), flag("deliberate"), flag("partialSkip"));
            // The three flavours put a test in three DIFFERENT places -- skip
            // leaves the denominator entirely, deliberate stays in it as a
            // failure, partialSkip is scored by exit code -- and each is
            // ratcheted by its own ceiling. Carrying two would make the
            // outcome depend on the order this parser happens to test them in,
            // which is exactly the silent-drop the ceilings exist to prevent.
            let carried: Vec<&str> = [
                ("skip", skip),
                ("deliberate", deliberate),
                ("partialSkip", partial),
            ]
            .into_iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| name)
            .collect();
            if carried.len() > 1 {
                bail!(
                    "manifest.json: \"{k}\" carries {} -- the per-test flavours are mutually \
                     exclusive. A `skip` leaves the denominator, a `deliberate` divergence stays \
                     in it as a failure, and a `partialSkip` is scored by its exit code; pick one.",
                    carried.join(" + ")
                );
            }
            let reason = |fallback: &str| {
                cfg.get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or(fallback)
                    .to_string()
            };
            if skip {
                let category = cfg.get("category").and_then(|c| c.as_str()).unwrap_or("");
                if category == "known_issues" || category == "flaky" {
                    m.known_issues += 1;
                }
                m.skips.insert(k.clone(), reason("manifest skip"));
            } else if deliberate {
                m.deliberate
                    .insert(k.clone(), reason("deliberate divergence"));
            } else if partial {
                m.partial_skips.insert(k.clone(), reason("partial skip"));
            }
        }
    }
    Ok(m)
}

/// The bail text for a corpus whose file count no longer matches
/// `manifest.expectedTotal`. Pure (and unit-tested) so the guidance a
/// once-a-year failure prints is not itself untested.
///
/// A bare integer can prove that the population MOVED but cannot name a file
/// that was added -- nothing in the repo enumerates the intended corpus, and
/// this check stays offline (no git, no network) so it works on a release host
/// with a detached tree. What it CAN name is the other direction: a manifest
/// entry (skip or deliberate) pointing at a test that is no longer on disk,
/// which is exactly the shape a dropped file leaves behind.
fn corpus_drift_message(on_disk: &[PathBuf], expected: usize, manifest: &Manifest) -> String {
    let names: BTreeSet<String> = on_disk
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let orphaned: Vec<&str> = manifest
        .skips
        .keys()
        .chain(manifest.deliberate.keys())
        .filter(|key| {
            let base = key.rsplit('/').next().unwrap_or(key);
            !names.contains(base)
        })
        .map(String::as_str)
        .collect();

    let found = on_disk.len();
    let drift = if found > expected {
        format!("{} MORE than expected", found - expected)
    } else {
        format!("{} FEWER than expected", expected - found)
    };
    let mut msg = format!(
        "corpus drift: {found} test files in conformance/vendor/node/test/parallel, \
         manifest expectedTotal = {expected} ({drift}). Every published rate divides by this \
         population, so a vendored file added or dropped moves the number with nothing to notice. \
         Re-vendor the corpus, or (with review) update expectedTotal in \
         conformance/vendor/node/manifest.json."
    );
    if !orphaned.is_empty() {
        msg.push_str(&format!(
            "\n  missing (manifest names it, no file on disk): {}",
            orphaned.join(", ")
        ));
    }
    msg.push_str(
        "\n  unexpected: a bare count cannot name an ADDED file -- list \
         conformance/vendor/node/test/parallel and compare it against the tracked tree.",
    );
    msg
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

/// The first line of captured output that says something about the FAILURE.
///
/// Node emits process warnings to stderr AHEAD of everything the run later
/// prints, so "first non-empty line of stderr" is not the same thing as "first
/// divergence". `require('internal/test/binding')` emits one on every load, and
/// it shadowed the real error for every test that touches that module -- in the
/// console AND in the CONFORMANCE-NODE.md failures section, which is the receipt
/// a reader triages from:
///
///     (node:13288) internal/test/binding: These APIs are for internal testing only. Do not use them.
///
/// Skipped shapes: `(node:<pid>) <text>`, its `[DEP0XXX] DeprecationWarning`
/// variant, and the `(Use \`node --trace-warnings ...\`)` follow-up node appends
/// to the first warning of a run. stderr still outranks stdout at BOTH levels:
/// a diagnostic line anywhere beats a warning, and a warning beats nothing --
/// if every line of both streams is a warning the first one is still reported,
/// because "no detail at all" is a worse receipt than a warning.
fn first_error_line(out: &Captured) -> String {
    let clip = |l: &str| l.chars().take(140).collect::<String>();
    let diagnostic = |s: &str| {
        let mut lines = s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !is_process_warning(l))
            .peekable();
        // Step over oam's code-frame preamble so the receipt records the
        // rendered error rather than the throw SITE. The frame is a bare
        // `<file>:<line>` header, the offending source line, then a caret
        // row; the message follows. Recording the header instead made the
        // committed receipt churn on any edit that MOVED the throw, and it
        // told a triager the least useful half of the failure -- `oam:
        // node_compat.js:7640` where `AssertionError [ERR_ASSERTION]:
        // Expected values to be strictly deep-equal` was two lines below.
        // A `<file>:<line>` header carrying no message of its own is the
        // only shape skipped, so an ordinary one-line error is untouched.
        if lines.peek().is_some_and(|l| is_code_frame_header(l)) {
            lines.next();
            // The source line, then the caret row. Both are best-effort:
            // a frame missing either one still lands on the message.
            if lines.peek().is_some_and(|l| !is_caret_row(l)) {
                lines.next();
            }
            if lines.peek().is_some_and(|l| is_caret_row(l)) {
                lines.next();
            }
        }
        lines.next().map(clip)
    };
    let any = |s: &str| s.lines().map(str::trim).find(|l| !l.is_empty()).map(clip);
    diagnostic(&out.stderr)
        .or_else(|| diagnostic(&out.stdout))
        .or_else(|| any(&out.stderr))
        .or_else(|| any(&out.stdout))
        .unwrap_or_default()
}

/// A node process-warning line: `(node:<pid>) ...` (which covers the
/// `[DEP0XXX] DeprecationWarning: ...` spelling too, same prefix) and the
/// `(Use \`node --trace-warnings ...\`)` hint that follows the first one.
/// The pid is required to be digits so a test printing a literal `(node:` of
/// its own does not get silently swallowed.
fn is_process_warning(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("(node:")
        && let Some(end) = rest.find(')')
        && !rest[..end].is_empty()
        && rest[..end].chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    line.starts_with("(Use `") && line.contains("--trace-warnings")
}

/// The first row of oam's code frame: a bare `<file>:<line>` location with no
/// message after it (`oam:node_compat.js:7640`, or an absolute path when the
/// throw is in the test file itself). Requiring the tail after the LAST colon
/// to be all digits, and nothing to follow it, is what keeps a real one-line
/// error such as `Error: connect ECONNREFUSED 127.0.0.1:8080` from matching:
/// that line has text before the location, so its last segment is not the
/// whole remainder.
fn is_code_frame_header(line: &str) -> bool {
    let Some((path, lineno)) = line.rsplit_once(':') else {
        return false;
    };
    !path.is_empty()
        && !lineno.is_empty()
        && lineno.chars().all(|c| c.is_ascii_digit())
        && !path.contains(char::is_whitespace)
}

/// The caret row under a code frame's source line: carets and spaces only.
fn is_caret_row(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|c| c == '^' || c == ' ')
}

/// Everything one run publishes into the two committed receipts.
///
/// Was eleven positional arguments behind an `#[allow(too_many_arguments)]`;
/// naming the exclusion lists took it to thirteen, at which point `total,
/// runnable, pass, fail, skip, unrunnable` was six bare integers in a row that
/// a caller could transpose silently. As a struct the counts that are DERIVED
/// stop being passed at all: `runnable` is `pass + fail`, `skip` is
/// `skips.len()`, `unrunnable` is `unrunnable.len()` -- three fewer places for
/// a published number to disagree with the list beside it.
struct Scorecard<'a> {
    by_module: &'a BTreeMap<String, [usize; 4]>,
    failures: &'a [(String, String)],
    deliberate: &'a [(String, String, String)],
    /// self-skips: (test, the reason it printed on the wire).
    skips: &'a [(String, String)],
    /// manifest partialSkip entries: (test, why its marker is not the verdict).
    /// These are scored by exit code, so they are already counted in `pass` or
    /// `fail` -- listed for the same reason the exclusions are, because the
    /// entry is a discretionary lever on the denominator.
    partial_skips: &'a [(String, String)],
    /// harness exclusions: (test, reason, why-class).
    unrunnable: &'a [(String, String, UnrunnableKind)],
    /// every test file in the corpus, scored or not.
    total: usize,
    pass: usize,
    fail: usize,
}

fn write_scorecard(repo: &Path, oam: &Path, card: &Scorecard) -> Result<bool> {
    let Scorecard {
        by_module,
        failures,
        deliberate,
        skips,
        partial_skips,
        unrunnable,
        total,
        pass,
        fail,
    } = *card;
    let oam_version = Command::new(oam)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let commit = crate::conformance::git_short_commit(repo);

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
        "runnable": scored,
        "pass": pass,
        "fail": fail,
        // Counts, kept for the existing readers; `skips` / `unrunnables` below
        // are the same two populations NAMED, and are the same length by
        // construction (both derive from these lists).
        "skip": skips.len(),
        "unrunnable": unrunnable.len(),
        "passOverRunnable": format!("{:.1}%", pct(pass, scored)),
        "passOverTotal": format!("{:.1}%", pct(pass, total)),
        "byModule": modules_json,
        // The 45-odd tests that left the denominator, by name. Previously only
        // a per-module integer, which meant a compensating change -- one test
        // starting to self-skip while another stopped -- moved nothing in the
        // receipt and nothing in minPass. Now it is a reviewable diff.
        "skips": skips
            .iter()
            .map(|(name, reason)| json!({ "test": name, "reason": reason }))
            .collect::<Vec<_>>(),
        "unrunnables": unrunnable
            .iter()
            .map(|(name, reason, kind)| json!({
                "test": name, "reason": reason, "kind": kind.as_str(),
            }))
            .collect::<Vec<_>>(),
        // NOT an excluded population: these are IN `runnable` and in pass/fail.
        // Published because the entry is the one lever that moves a test the
        // other way -- over its own TAP skip marker -- so it has to be as
        // auditable as the levers that move tests out.
        "partialSkips": partial_skips
            .iter()
            .map(|(name, reason)| json!({ "test": name, "reason": reason }))
            .collect::<Vec<_>>(),
        // A SUBSET of `fail`, not a sibling of it: these are counted in `fail`
        // and in `runnable` above. Published so the machine twin carries the
        // reason too, rather than leaving it only in the markdown.
        "deliberateFailures": deliberate
            .iter()
            .map(|(name, detail, reason)| json!({
                "test": name, "firstDivergence": detail, "reason": reason,
            }))
            .collect::<Vec<_>>(),
    });
    // Held for the grouped write at the end -- see conformance::write_receipts.
    let scorecard_json = serde_json::to_string_pretty(&scorecard)?;

    let mut md = String::new();
    md.push_str("# oam Node-suite scorecard\n\n");
    md.push_str("Generated by `cargo run -p xtask -- node-suite` -- do not edit by hand.\n");
    md.push_str("Machine twin: [`conformance/node-suite-scorecard.json`](conformance/node-suite-scorecard.json).\n\n");
    md.push_str(&format!(
        "Commit `{commit}` | {oam_version} | Node v22.22.2 | host {}-{}\n\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    md.push_str("Oracle: a vendored Node core test passes when it runs to **exit 0** (Node tests self-assert via `require('../common')` + `assert`). `1..0 # Skipped` reclassifies a runtime skip, except for the `partialSkip` entries listed below; `// Flags:` and manifest entries are unrunnable-by-harness.\n\n");
    md.push_str(&format!(
        "**pass/runnable = {pass}/{scored} ({:.1}%)** &nbsp; pass/total = {pass}/{total} ({:.1}%)\n\n",
        pct(pass, scored),
        pct(pass, total)
    ));
    md.push_str(&format!(
        "{pass} pass &middot; {fail} fail &middot; {} skip &middot; {} unrunnable-by-harness \
         (both excluded populations are named in full below)\n\n",
        skips.len(),
        unrunnable.len()
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
    if !deliberate.is_empty() {
        md.push_str("\n## Deliberate divergences (counted as failures above)\n\n");
        md.push_str(
            "These fail on purpose and are expected to keep failing. They are NOT skipped: each \
             one runs, counts as a failure, and stays in the denominator, so the pass rate above \
             is unaffected by this section existing. Listed separately only so a settled decision \
             is not mistaken for untriaged work.\n\n",
        );
        for (name, detail, reason) in deliberate {
            md.push_str(&format!("- `{name}` -- {detail}\n  - {reason}\n"));
        }
    }
    if !partial_skips.is_empty() {
        md.push_str("\n## Partial skips (scored by exit code, INSIDE the denominator)\n\n");
        md.push_str(
            "Each of these prints a TAP `1..0 # Skipped` marker part-way through -- \
             `common.printSkipMessage()` does not exit -- and then keeps asserting to the end of \
             the file, so the marker does not describe how the run ended. For these the marker is \
             ignored and the exit code decides, which lands them in pass or fail above instead of \
             the self-skipped list below. No output-only rule can tell such a test from one that \
             `common.skip()`s at the top and asserts nothing (their stdout is byte-identical), so \
             this is a per-file manifest opt-in, ratcheted by `maxPartialSkips`, not a heuristic.\n\n",
        );
        for (name, reason) in partial_skips {
            md.push_str(&format!("- `{name}` -- {reason}\n"));
        }
    }
    // The two excluded populations, by name. Failures and deliberate
    // divergences were already named in full; these were per-module integers,
    // so nobody reading the receipt could audit WHICH tests left the
    // denominator -- and a swap (one test starts self-skipping, another stops)
    // was invisible to every published number, minPass included.
    if !skips.is_empty() {
        md.push_str("\n## Self-skipped at runtime (outside the denominator)\n\n");
        md.push_str(
            "Each of these printed a TAP `1..0 # Skipped` marker AND exited 0, so it reached no \
             verdict about oam and is not counted in pass/runnable. The reason is the one the \
             test itself put on the wire. A skip here is usually the test declining a platform \
             or a build option -- but a skip that appears WITHOUT a corresponding change to the \
             test is a regression in whatever it feature-detects, so this list is worth diffing.\n\n",
        );
        for (name, reason) in skips {
            md.push_str(&format!("- `{name}` -- {reason}\n"));
        }
    }
    if !unrunnable.is_empty() {
        md.push_str("\n## Unrunnable by harness (outside the denominator)\n\n");
        md.push_str(
            "Excluded before or during the run, by kind: `manifest` (an explicit entry in \
             conformance/vendor/node/manifest.json -- the one discretionary lever, ratcheted by \
             maxSkips), `flags` (a `// Flags:` header naming a flag oam does not implement), \
             `missing-module` (the test imports a `node:`/`internal/` module, or an \
             internalBinding namespace, that oam does not provide -- an oam-side gap, not a \
             correctness failure, since the test never reaches an assertion about oam).\n\n",
        );
        for (name, reason, kind) in unrunnable {
            md.push_str(&format!("- `{name}` -- [{}] {reason}\n", kind.as_str()));
        }
    }
    crate::conformance::write_receipts(&[
        (
            repo.join("conformance/node-suite-scorecard.json"),
            scorecard_json,
        ),
        (repo.join("CONFORMANCE-NODE.md"), md),
    ])
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

    /// A finished attempt: what the test printed on stdout, and how it ended.
    fn attempt(stdout: &str, code: i32, timed_out: bool) -> Captured {
        Captured {
            stdout: stdout.to_string(),
            stderr: String::new(),
            code,
            timed_out,
        }
    }

    fn manifest_naming(paths: &[&str]) -> Manifest {
        Manifest {
            skips: paths
                .iter()
                .map(|p| ((*p).to_string(), "because".to_string()))
                .collect(),
            known_issues: 0,
            deliberate: BTreeMap::new(),
            partial_skips: BTreeMap::new(),
            max_skips: None,
            max_known_issues: None,
            max_deliberate: None,
            max_partial_skips: None,
            min_pass: None,
            min_pass_by_host: BTreeMap::new(),
            expected_total: None,
        }
    }

    /// Every ceiling and floor enforced, at the values the suite runs with, so
    /// each test below overrides only the one lever it is about. Named fields
    /// mean a test cannot silently transpose a count with a limit.
    fn ratchet() -> Ratchet {
        Ratchet {
            skips: 0,
            known_issues: 0,
            deliberate: 0,
            partial_skips: 0,
            pass: 81,
            max_skips: Some(0),
            max_known_issues: Some(0),
            max_deliberate: Some(0),
            max_partial_skips: Some(0),
            min_pass: Some(81),
        }
    }

    // -- skip-ratchet: the integrity mechanism against denominator-gaming --

    #[test]
    fn ratchet_within_or_at_ceiling_is_ok() {
        assert!(ratchet_violation(&ratchet()).is_none());
        assert!(
            ratchet_violation(&Ratchet {
                skips: 3,
                known_issues: 1,
                deliberate: 1,
                partial_skips: 1,
                pass: 90,
                max_skips: Some(5),
                max_known_issues: Some(2),
                max_deliberate: Some(2),
                max_partial_skips: Some(2),
                min_pass: Some(81),
            })
            .is_none()
        );
        // == ceiling / == floor is allowed; only strictly past them violates.
        assert!(
            ratchet_violation(&Ratchet {
                skips: 5,
                known_issues: 2,
                deliberate: 2,
                partial_skips: 2,
                pass: 81,
                max_skips: Some(5),
                max_known_issues: Some(2),
                max_deliberate: Some(2),
                max_partial_skips: Some(2),
                min_pass: Some(81),
            })
            .is_none()
        );
        // Unset ceilings/floor = unenforced, even with extreme counts.
        assert!(
            ratchet_violation(&Ratchet {
                skips: 99,
                known_issues: 99,
                deliberate: 99,
                partial_skips: 99,
                pass: 0,
                ..Ratchet::default()
            })
            .is_none()
        );
    }

    #[test]
    fn ratchet_skips_over_ceiling_violates() {
        let msg = ratchet_violation(&Ratchet {
            skips: 1,
            min_pass: None,
            ..ratchet()
        })
        .expect("must violate");
        assert!(msg.contains("skip-ratchet violation"));
        assert!(msg.contains("1 manifest skips > ceiling 0"));
    }

    #[test]
    fn ratchet_known_issues_over_ceiling_violates() {
        let msg = ratchet_violation(&Ratchet {
            known_issues: 3,
            max_skips: Some(10),
            max_known_issues: Some(2),
            min_pass: None,
            ..ratchet()
        })
        .expect("must violate");
        assert!(msg.contains("known-issues ceiling violation"));
        assert!(msg.contains("3 known_issues/flaky skips > ceiling 2"));
    }

    #[test]
    fn deliberate_ceiling_is_ratcheted() {
        // At the ceiling is fine; one past it is not. This is what stops a real
        // regression being relabelled "intentional" without a reviewable bump.
        assert!(
            ratchet_violation(&Ratchet {
                deliberate: 2,
                max_deliberate: Some(2),
                ..ratchet()
            })
            .is_none()
        );
        let msg = ratchet_violation(&Ratchet {
            deliberate: 3,
            max_deliberate: Some(2),
            ..ratchet()
        })
        .expect("must violate");
        assert!(msg.contains("deliberate-divergence ceiling violation"));
        assert!(msg.contains("3 > ceiling 2"));
    }

    #[test]
    fn deliberate_does_not_shrink_the_denominator() {
        // The guarantee the classification rests on: a deliberate divergence is
        // a FAILURE, not a skip. If it ever stopped counting, the pass rate
        // would silently improve by annotating tests -- exactly what the skip
        // ceiling exists to prevent. A pass floor must still fire underneath it.
        let msg = ratchet_violation(&Ratchet {
            deliberate: 2,
            max_deliberate: Some(2),
            pass: 80,
            ..ratchet()
        })
        .expect("must violate");
        assert!(msg.contains("pass-floor violation"));
    }

    #[test]
    fn partial_skip_ceiling_is_ratcheted_separately_from_maxskips() {
        // At the ceiling is fine.
        assert!(
            ratchet_violation(&Ratchet {
                partial_skips: 1,
                max_partial_skips: Some(1),
                ..ratchet()
            })
            .is_none()
        );
        // One past it is not: a partialSkip promotes a marker-printing test
        // back INTO the scored denominator, which is exactly the direction a
        // ceiling has to govern.
        let msg = ratchet_violation(&Ratchet {
            partial_skips: 2,
            max_partial_skips: Some(1),
            ..ratchet()
        })
        .expect("must violate");
        assert!(msg.contains("partial-skip ceiling violation"), "{msg}");
        assert!(msg.contains("2 > ceiling 1"), "{msg}");
        // And it must NOT be governed by maxSkips, which is 0 and means the
        // opposite thing (no test may LEAVE the denominator by fiat). A
        // partialSkip under a generous maxPartialSkips passes with maxSkips 0.
        assert!(
            ratchet_violation(&Ratchet {
                partial_skips: 1,
                max_skips: Some(0),
                max_partial_skips: Some(1),
                ..ratchet()
            })
            .is_none()
        );
    }

    #[test]
    fn ratchet_pass_below_floor_violates() {
        // A node-compat regression that drops the pass count must redden CI.
        let msg = ratchet_violation(&Ratchet {
            pass: 80,
            ..ratchet()
        })
        .expect("must violate");
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

    #[test]
    fn missing_builtin_extracts_unbacked_internal_binding() {
        // The throwing internalBinding proxy in js/node_compat.js. An unbacked
        // binding must stay UNRUNNABLE: it is an oam gap the test never gets
        // past, not a divergence the test measured.
        let c = cap(
            "Error: oam: no native binding for 'tcp_wrap.TCP'\n    at internalBinding (node:internal/test/binding:3:9)",
        );
        assert_eq!(missing_builtin(&c).as_deref(), Some("tcp_wrap.TCP"));
    }

    #[test]
    fn missing_builtin_names_the_binding_even_when_the_message_is_truncated() {
        let c = cap("Error: oam: no native binding for 'fs_event_wrap");
        assert_eq!(
            missing_builtin(&c).as_deref(),
            Some("an internalBinding namespace")
        );
    }

    #[test]
    fn missing_builtin_ignores_a_generic_module_error() {
        // Scoped to the DISTINCTIVE prefix on purpose. A bare "No such module"
        // is what a genuine oam bug looks like too, and matching it would move
        // real failures out of the denominator.
        assert!(missing_builtin(&cap("Error: No such module")).is_none());
        assert!(missing_builtin(&cap("Error: no native binding available")).is_none());
    }

    // -- classify_attempt: the TAP marker is NOT authoritative over the exit --

    #[test]
    fn marker_with_exit_zero_is_a_skip_carrying_its_reason() {
        let out = attempt("1..0 # Skipped: no crypto\n", 0, false);
        assert_eq!(
            classify_attempt(&out, false),
            Outcome::Skip("no crypto".to_string())
        );
    }

    #[test]
    fn a_partial_skip_that_finishes_is_still_scored_a_skip_by_default() {
        // Documents why the manifest opt-in exists, measured rather than
        // assumed. On oam these two produce BYTE-IDENTICAL output --
        // `1..0 # Skipped: missing crypto` on stdout, nothing else, exit 0:
        //
        //   test-stream-pipeline-http2.js  common.skip('missing crypto') at
        //                                  the top; asserts NOTHING.
        //   test-buffer-alloc.js           common.printSkipMessage() at :1076
        //                                  (no exit), then ~120 more lines of
        //                                  Buffer assertions, then exit 0.
        //
        // (Both because common.hasCrypto is `Boolean(process.versions.openssl)`
        // -- common/index.js:54 -- and oam publishes the deps it really links.)
        // So NO output-only rule separates them, and scoring exit-0-with-marker
        // as a Pass in general would score the first test -- which asserted
        // nothing -- as a pass. Skip stays the default for both.
        let ran_on = attempt("1..0 # Skipped: missing crypto\n", 0, false);
        let skipped_out = attempt("1..0 # Skipped: missing crypto\n", 0, false);
        assert_eq!(
            classify_attempt(&ran_on, false),
            classify_attempt(&skipped_out, false)
        );
        assert!(matches!(classify_attempt(&ran_on, false), Outcome::Skip(_)));
    }

    #[test]
    fn a_manifest_partial_skip_is_scored_by_its_exit_code() {
        // The opt-in half of the pair above: the marker is not consulted, so
        // the SAME bytes that scored Skip now score by how the process ended.
        let finished = attempt("1..0 # Skipped: missing crypto\n", 0, false);
        assert_eq!(classify_attempt(&finished, true), Outcome::Pass);

        // Overriding the marker must not override the exit code: a partialSkip
        // that starts failing has to fail, or the entry would be a blanket
        // "score this file a pass".
        let died = attempt("1..0 # Skipped: missing crypto\n", 1, false);
        match classify_attempt(&died, true) {
            Outcome::Fail(detail) => assert!(detail.contains("[exit=1]"), "{detail}"),
            other => panic!("a failing partialSkip must FAIL, got {other:?}"),
        }
        let hung = attempt("1..0 # Skipped: missing crypto\n", -2, true);
        assert_eq!(
            classify_attempt(&hung, true),
            Outcome::Fail("[TIMEOUT]".to_string())
        );
    }

    #[test]
    fn marker_with_nonzero_exit_is_a_failure() {
        // The process printed the marker and then died. Under the old ordering
        // this was scored a Skip and vanished from the denominator.
        let out = attempt("1..0 # Skipped: no ipv6\nAssertionError\n", 1, false);
        match classify_attempt(&out, false) {
            Outcome::Fail(detail) => assert!(detail.contains("[exit=1]"), "{detail}"),
            other => panic!("marker + non-zero exit must FAIL, got {other:?}"),
        }
    }

    #[test]
    fn marker_with_timeout_is_a_failure() {
        let out = attempt("1..0 # Skipped: whatever\n", -2, true);
        assert_eq!(
            classify_attempt(&out, false),
            Outcome::Fail("[TIMEOUT]".to_string())
        );
    }

    #[test]
    fn clean_exit_without_a_marker_is_a_pass() {
        assert_eq!(
            classify_attempt(&attempt("ok\n", 0, false), false),
            Outcome::Pass
        );
    }

    #[test]
    fn missing_module_outranks_a_plain_failure_but_not_the_exit_code() {
        let mut out = attempt("", 1, false);
        out.stderr = "error[OAM-MOD0006]: 'node:test' is not a known node: builtin module".into();
        assert_eq!(
            classify_attempt(&out, false),
            Outcome::Unrunnable(
                "needs unimplemented node:test".to_string(),
                UnrunnableKind::MissingModule
            )
        );
        // ... but a timeout is still a failure, not an exclusion.
        out.timed_out = true;
        assert_eq!(
            classify_attempt(&out, false),
            Outcome::Fail("[TIMEOUT]".to_string())
        );
    }

    // -- skip_marker_reason: the reason common.skip() already puts on the wire --

    #[test]
    fn skip_reason_survives_both_marker_spellings() {
        assert_eq!(
            skip_marker_reason("1..0 # Skipped: missing crypto\n").as_deref(),
            Some("missing crypto")
        );
        // "1..0 # SKIP" is a PREFIX of "1..0 # Skipped" -- matching it first
        // would leave the reason as "ped: missing crypto".
        assert_eq!(
            skip_marker_reason("1..0 # SKIP no ipv6 support\n").as_deref(),
            Some("no ipv6 support")
        );
        // Windows CRLF must not ride along into the receipt.
        assert_eq!(
            skip_marker_reason("1..0 # Skipped: no ipv6\r\n").as_deref(),
            Some("no ipv6")
        );
    }

    #[test]
    fn skip_reason_falls_back_when_the_test_printed_none() {
        assert_eq!(
            skip_marker_reason("1..0 # Skipped\n").as_deref(),
            Some("no reason given")
        );
        assert_eq!(skip_marker_reason("all good\n"), None);
    }

    // -- corpus pin: the denominator is a committed number, not whatever is on disk --

    #[test]
    fn corpus_drift_message_reports_the_direction_and_the_orphans() {
        let on_disk = [PathBuf::from("test/parallel/test-buffer-alloc.js")];
        let manifest = manifest_naming(&["parallel/test-buffer-alloc.js", "parallel/test-gone.js"]);

        let msg = corpus_drift_message(&on_disk, 2, &manifest);
        assert!(msg.contains("1 test files"), "{msg}");
        assert!(msg.contains("expectedTotal = 2"), "{msg}");
        assert!(msg.contains("1 FEWER than expected"), "{msg}");
        // The one side a bare count CAN name: a manifest entry with no file.
        assert!(msg.contains("parallel/test-gone.js"), "{msg}");
        assert!(!msg.contains("test-buffer-alloc.js\n"), "{msg}");

        let msg = corpus_drift_message(&on_disk, 0, &manifest);
        assert!(msg.contains("1 MORE than expected"), "{msg}");
    }

    #[test]
    fn corpus_pin_reads_expected_total_from_either_home() {
        let dir =
            std::env::temp_dir().join(format!("oam-node-suite-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");

        let top = dir.join("top.json");
        std::fs::write(&top, r#"{"expectedTotal": 476, "tests": {}}"#).expect("write");
        assert_eq!(
            load_manifest(&top).expect("parse").expected_total,
            Some(476)
        );

        // Filed with the other numbers instead -- still checked, not silently
        // ignored, because a pin that quietly does nothing is worse than none.
        let nested = dir.join("nested.json");
        std::fs::write(&nested, r#"{"ratchet": {"expectedTotal": 12}}"#).expect("write");
        assert_eq!(
            load_manifest(&nested).expect("parse").expected_total,
            Some(12)
        );

        // Absent = unpinned (the check is opt-in).
        let bare = dir.join("bare.json");
        std::fs::write(&bare, r#"{"tests": {}}"#).expect("write");
        assert_eq!(load_manifest(&bare).expect("parse").expected_total, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- partialSkip: the reviewable opt-in that overrides a TAP marker --

    #[test]
    fn manifest_parses_the_three_test_flavours_into_separate_buckets() {
        let dir = std::env::temp_dir().join(format!(
            "oam-node-suite-flavours-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("manifest.json");
        std::fs::write(
            &path,
            r#"{
              "ratchet": { "maxSkips": 0, "maxPartialSkips": 1 },
              "tests": {
                "parallel/test-a.js": { "skip": true, "reason": "no fixture", "category": "flaky" },
                "parallel/test-b.js": { "deliberate": true, "reason": "honest surface" },
                "parallel/test-c.js": { "partialSkip": true, "reason": "asserts past its marker" }
              }
            }"#,
        )
        .expect("write");

        let m = load_manifest(&path).expect("parse");
        assert_eq!(m.skips.len(), 1);
        assert_eq!(m.known_issues, 1);
        assert_eq!(m.deliberate.len(), 1);
        assert_eq!(
            m.partial_skips
                .get("parallel/test-c.js")
                .map(String::as_str),
            Some("asserts past its marker")
        );
        assert_eq!(m.max_partial_skips, Some(1));
        // maxPartialSkips is its own lever, NOT maxSkips (which is 0 and means
        // the opposite thing) -- reusing it would let one be traded for the
        // other with nothing to review.
        assert_eq!(m.max_skips, Some(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_rejects_a_test_carrying_two_flavours() {
        let dir = std::env::temp_dir().join(format!(
            "oam-node-suite-conflict-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("manifest.json");
        // Silently letting `skip` win would drop the test out of the
        // denominator while the entry claimed it was being scored.
        std::fs::write(
            &path,
            r#"{"tests": {"parallel/test-x.js": {"skip": true, "partialSkip": true}}}"#,
        )
        .expect("write");

        // `expect_err` would need Debug on Manifest, which lives here only for
        // this assertion -- match instead.
        let msg = match load_manifest(&path) {
            Ok(_) => panic!("a test carrying two flavours must be an error, not a precedence rule"),
            Err(e) => e.to_string(),
        };
        assert!(msg.contains("parallel/test-x.js"), "{msg}");
        assert!(msg.contains("mutually exclusive"), "{msg}");
        assert!(msg.contains("skip + partialSkip"), "{msg}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- no_binding_name: read the THROWN message, never the echoed source --

    #[test]
    fn no_binding_name_ignores_the_code_frame_and_reads_the_message() {
        // oam's real fatal report, verbatim: the code frame echoes the throw
        // site's SOURCE (template literal intact) ahead of the rendered error.
        // A substring search over this extracted "${id}" and the committed
        // receipt read "needs unimplemented ${id}".
        let c = cap(concat!(
            "oam:node_compat.js:22667\n",
            "        throw new Error(`oam: no native binding for '${id}'`);\n",
            "        ^\n",
            "\n",
            "Error: oam: no native binding for 'js_stream'\n",
            "    at internalBinding (oam:node_compat.js:22667:15)\n",
        ));
        assert_eq!(missing_builtin(&c).as_deref(), Some("js_stream"));

        // Same shape for the proxy's per-member throw, whose source line
        // carries TWO interpolations.
        let c = cap(concat!(
            "oam:node_compat.js:22607\n",
            "          throw new Error(`oam: no native binding for '${ns}.${String(prop)}'`);\n",
            "          ^\n",
            "\n",
            "Error: oam: no native binding for 'timers.scheduleTimer'\n",
        ));
        assert_eq!(missing_builtin(&c).as_deref(), Some("timers.scheduleTimer"));
    }

    #[test]
    fn no_binding_name_still_reads_a_message_only_report() {
        // The shape that always worked -- it must not regress. Includes the
        // nested `actual: Error: ...` form util.inspect prints when the binding
        // error is captured inside an AssertionError diff (test-buffer-fill),
        // which is how that one test extracted correctly while four did not.
        assert_eq!(
            missing_builtin(&cap("Error: oam: no native binding for 'tcp_wrap.TCP'")).as_deref(),
            Some("tcp_wrap.TCP")
        );
        assert_eq!(
            missing_builtin(&cap(
                "  actual: Error: oam: no native binding for 'buffer.fill'"
            ))
            .as_deref(),
            Some("buffer.fill")
        );
    }

    #[test]
    fn no_binding_name_never_publishes_an_uninterpolated_template() {
        // Classification must NOT depend on the parse: a report carrying only
        // the code frame is still unrunnable (an oam gap the test never got
        // past), it is just unnameable. What must never happen is `${id}`
        // reaching the receipt as a binding name.
        let c = cap("        throw new Error(`oam: no native binding for '${id}'`);");
        assert_eq!(
            missing_builtin(&c).as_deref(),
            Some("an internalBinding namespace")
        );
    }

    // -- first_error_line: a process warning is not the first divergence --

    #[test]
    fn first_error_line_skips_node_process_warnings() {
        // require('internal/test/binding') emits this on every load, so it
        // shadowed the real error for every test that touches that module --
        // in the console AND in CONFORMANCE-NODE.md's failures section.
        let c = cap(concat!(
            "(node:13288) internal/test/binding: These APIs are for internal testing only. Do not use them.\n",
            "(node:13288) [DEP0040] DeprecationWarning: The `punycode` module is deprecated.\n",
            "(Use `node --trace-warnings ...` to show where the warning was created)\n",
            "AssertionError [ERR_ASSERTION]: mismatch: false vs true for Uint8Array\n",
        ));
        assert_eq!(
            first_error_line(&c),
            "AssertionError [ERR_ASSERTION]: mismatch: false vs true for Uint8Array"
        );
    }

    #[test]
    fn first_error_line_steps_over_oams_code_frame() {
        // Verbatim shape of `oam test-process-versions.js` on this tree. The
        // header alone is what the receipt used to record, which both churned
        // the committed file whenever an unrelated edit moved the throw and
        // told a triager nothing about the failure.
        let c = cap(concat!(
            "oam:node_compat.js:7640\n",
            "      throw new AssertionError({ actual, expected, message, operator, stackStartFn });\n",
            "      ^\n",
            "\n",
            "AssertionError [ERR_ASSERTION]: Expected values to be strictly deep-equal:\n",
        ));
        assert_eq!(
            first_error_line(&c),
            "AssertionError [ERR_ASSERTION]: Expected values to be strictly deep-equal:"
        );
    }

    #[test]
    fn first_error_line_steps_over_a_code_frame_behind_warnings() {
        // The two fixes compose: warnings are dropped first, then the frame.
        let c = cap(concat!(
            "(node:8212) internal/test/binding: These APIs are for internal testing only.\n",
            "C:\\repo\\conformance\\vendor\\node\\test\\parallel\\test-x.js:42\n",
            "  assert.strictEqual(a, b);\n",
            "  ^\n",
            "\n",
            "AssertionError [ERR_ASSERTION]: mismatch: false vs true for Uint8Array\n",
        ));
        assert_eq!(
            first_error_line(&c),
            "AssertionError [ERR_ASSERTION]: mismatch: false vs true for Uint8Array"
        );
    }

    #[test]
    fn first_error_line_keeps_a_one_line_error_that_ends_in_a_port() {
        // The frame header is `<path>:<line>` and NOTHING else. An ordinary
        // error whose message merely ends in `:<digits>` must survive, or the
        // skip would eat the only line the receipt has.
        let c = cap("Error: connect ECONNREFUSED 127.0.0.1:8080\n");
        assert_eq!(
            first_error_line(&c),
            "Error: connect ECONNREFUSED 127.0.0.1:8080"
        );
    }

    #[test]
    fn first_error_line_survives_a_frame_with_nothing_after_it() {
        // A truncated frame (killed process, clipped output) must not fall
        // through to an empty detail -- report what there is.
        let c = cap("oam:node_compat.js:7640\n      throw new Error('x');\n      ^\n");
        assert!(!first_error_line(&c).is_empty());
    }

    #[test]
    fn first_error_line_falls_back_when_every_line_is_a_warning() {
        // No detail at all is a worse receipt than a warning, so the warning
        // still gets reported rather than an empty string.
        let c = cap("(node:1) internal/test/binding: These APIs are for internal testing only.\n");
        let line = first_error_line(&c);
        assert!(line.starts_with("(node:1)"), "{line}");

        // A diagnostic line on stdout still outranks a warning-only stderr.
        let mut c =
            cap("(node:1) internal/test/binding: These APIs are for internal testing only.\n");
        c.stdout = "not ok 3 - buffer fill\n".into();
        assert_eq!(first_error_line(&c), "not ok 3 - buffer fill");
    }

    #[test]
    fn first_error_line_does_not_swallow_a_tests_own_parenthesised_output() {
        // The pid must be digits: `(node:foo)` is a test printing, not a
        // warning, and silently dropping it would hide a real divergence.
        let c = cap("(node:foo) something the test itself printed\n");
        assert_eq!(
            first_error_line(&c),
            "(node:foo) something the test itself printed"
        );
        assert!(!is_process_warning("(Use `strict`) not a warning hint"));
    }
}
