#!/usr/bin/env node
// =============================================================================
// MCP sidecar regression matrix -- does each sidecar still BOOT, SERVE TOOLS,
// and ANSWER a tool call when hosted on oam?
// =============================================================================
// Yaw MCP defaults `runtime: "oam"`, so an oam release that breaks a sidecar
// breaks the broker for real users. The 2026-06-28 11/12 validation was
// one-time and manual; shipping oam as the default runtime needs prevention,
// not a fallback that fires after the damage.
//
// What it does per sidecar, which is the point: it reproduces the spawn Yaw MCP
// actually performs (oam-spawn.ts) rather than approximating it --
//
//     npx [-y] <pkg> [...rest]   ->   oam run <resolved bin> [-- ...rest]
//
// resolving the package's REAL bin from its package.json, the way the broker
// does. A harness that invented its own launch could pass while production
// fails. Then it speaks MCP over stdio: initialize, notifications/initialized,
// tools/list -- and requires a non-empty tool list. Booting is not enough; a
// sidecar that starts and serves nothing is still broken.
//
// Serving a tool list is not enough EITHER. A sidecar whose every tool throws
// the moment it is invoked advertises all of them and ships green: tools/list
// is answered by the server's registration table, which can be intact while
// everything behind it is broken. So a sidecar that HAS a tool needing no
// credential, no network and no external service gets that tool CALLED, and
// the result asserted. Which sidecars those are is a property of what they
// actually advertise: four of them expose nothing but calls into a credentialed
// API or a live database, and two drive a real browser, so no such tool exists
// to call. Those are reported BOOT-ONLY, by name, on every run -- a gate that
// quietly counted them as covered would read as more coverage than it has.
//
// Every invocation is ALSO run on node, and the two outcomes are adjudicated
// together. A tool that fails the same way on both is a BROKEN SIDECAR, not an
// oam regression, and the gate says so rather than failing an oam release for
// somebody else's publish. That distinction is the difference between a gate
// people trust and one they learn to skip.
//
// The control arm is the expensive half -- node boots these sidecars several
// times slower than oam does, which is the thing oam is FOR -- and it roughly
// quintuples a warm probe phase (2.3s to 13.2s for the three invocable
// sidecars, measured on win32-arm64). It stays on anyway: the probe phase is
// seconds against an npm install measured in minutes, and a control taken only
// when something is already red is a control nobody trusts.
//
// Runs on NODE, deliberately: it is testing oam, and a harness hosted on the
// runtime under test turns "oam is broken" into "the harness is broken". The
// node control arm is the same fact used twice -- the harness's own executable
// is the reference implementation, already on the box, already trusted.
//
// Usage:
//   node scripts/mcp-sidecar-matrix.mjs                 # every oam-hosted sidecar
//   node scripts/mcp-sidecar-matrix.mjs --only=fetch,memory
//   node scripts/mcp-sidecar-matrix.mjs --list
//   node scripts/mcp-sidecar-matrix.mjs --self-test     # checks THIS harness;
//                                                       # no network, no npm, no oam
//   OAM_BIN=/path/to/oam node scripts/mcp-sidecar-matrix.mjs
//
// Exit code is the gate: 0 only when every selected sidecar served tools and
// every sidecar that has a callable tool answered it.
// =============================================================================

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";

// Paths only -- nothing here touches the disk. `--list` and `--self-test` run
// where there is no stage, no npm and no oam, and both are answered before the
// first mkdir below.
const stage = join(tmpdir(), "oam-mcp-matrix");
// Tool-call inputs. Under the stage so one directory holds everything the run
// creates, and wiped at the start of each run: a sidecar that WROTE here would
// otherwise make the next run's assertion depend on the previous run's output.
const fixture = join(stage, "fixture");
const FIXTURE_CONTEXT_FILE = "CLAUDE.md";
const FIXTURE_CONTEXT_BODY =
  "# oam sidecar matrix fixture\n\nFixed input so a token count is deterministic.\n";

// The oam-hosted set from bundles.json. `github` is the only exclusion left:
// it is docker-hosted, and the rewrite only ever touches node/npx launches.
//
// lemonsqueezy and ctxlint used to be excluded as "not opted in" -- true when
// `runtime: "oam"` had to be set per server, and stale since Yaw MCP 0.74.1
// made an UNSET runtime resolve to oam. Neither carries the key, and both are
// oam-hosted today, so leaving them out meant the release gate under-tested the
// real set by two.
//
// `args` are the launch args that follow the package spec, and ctxlint is the
// only sidecar that has any (`npx -y @yawlabs/ctxlint@latest serve`). That
// makes it the only entry exercising the `--` separator the rewrite emits, so
// it is doing double duty here: without it the harness never sends script args
// at all, and `oam run <entry> -- <args>` reaching a live stdio MCP server goes
// untested end to end (the argv plumbing alone is covered by e2e.rs).
//
// `env` supplies the configuration a sidecar refuses to START without. These
// are placeholders pointing at nothing, and that is fine: the matrix asks
// "does oam host this and does it serve its tools", which needs no live
// backend. Without them a sidecar exits on a missing variable and the matrix
// would report a FAILURE AGAINST OAM for a missing env var -- blaming the
// runtime for the harness's own gap, which is worse than not testing it.
//
// EVERY entry then declares exactly one of two things, and the self-test
// enforces that so a sidecar added later cannot slip in undecided:
//
//   `call`     -- a tool that needs no credential, no network and no external
//                 service, the arguments to call it with, and an assertion the
//                 result must satisfy. Run on oam AND on node.
//   `bootOnly` -- the honest reason no such tool exists. Printed on every run.
//
// The `bootOnly` reasons are claims about the sidecar's ADVERTISED surface,
// re-checked against tools/list output, not guesses: tailscale's 96 tools are
// 96 Tailscale API calls, lemonsqueezy's 64 are Lemon Squeezy API calls, every
// postgres and redis tool dials its server, and both browser sidecars need a
// browser binary installed out of band and leave a browser process behind.
const SIDECARS = [
  {
    name: "memory",
    pkg: "@modelcontextprotocol/server-memory",
    call: {
      tool: "read_graph",
      // A store path that does not exist yet, one per host: read_graph on a
      // missing store returns the empty graph, so the assertion is about the
      // shape the sidecar produces rather than about what some earlier run
      // left on disk. Per-host so the two arms cannot observe each other.
      env: (ctx) => ({ MEMORY_FILE_PATH: join(fixture, `memory-${ctx.host}.json`) }),
      args: () => ({}),
      deterministic: true,
      expect: (text) => {
        let graph;
        try {
          graph = JSON.parse(text);
        } catch {
          return "read_graph returned text that is not the JSON graph";
        }
        return Array.isArray(graph.entities) && Array.isArray(graph.relations)
          ? null
          : "read_graph returned a graph with no entities/relations arrays";
      },
    },
  },
  {
    name: "fetch",
    pkg: "@yawlabs/fetch-mcp",
    call: {
      tool: "http_get",
      // All 15 tools are HTTP, so "no network" is met by giving the sidecar a
      // server the harness itself starts on loopback: nothing external, no
      // name resolution, no credential. It is also the strongest call in the
      // set -- a real request/response through the sidecar's HTTP stack on
      // oam, asserted on the body that comes back.
      //
      // `allow_private_hosts` is not a test-only escape hatch; it is the
      // sidecar's own documented switch for exactly this, and it defaults off
      // so a loopback URL is refused without it.
      needsLoopback: true,
      args: (ctx) => ({
        url: ctx.loopbackUrl,
        allow_private_hosts: true,
        timeout_ms: 10_000,
      }),
      // The reply carries a Date header and a measured duration, so the two
      // arms cannot be compared byte for byte.
      deterministic: false,
      expect: (text) =>
        /^HTTP\/1\.1 200 OK/.test(text) && text.includes(LOOPBACK_MARKER)
          ? null
          : "http_get did not report a 200 carrying the fixture server's body",
    },
  },
  {
    name: "tailscale",
    pkg: "@yawlabs/tailscale-mcp",
    bootOnly: "all 96 tools call the Tailscale API with a tailnet credential",
  },
  {
    name: "postgres",
    pkg: "@yawlabs/postgres-mcp",
    bootOnly: "every tool runs against a live PostgreSQL connection",
  },
  {
    name: "redis",
    pkg: "@yawlabs/redis-mcp",
    env: { REDIS_URL: "redis://127.0.0.1:6379" },
    bootOnly: "every tool runs against a live Redis connection",
  },
  {
    name: "puppeteer",
    pkg: "@modelcontextprotocol/server-puppeteer",
    bootOnly: "every tool drives a browser that installs out of band and outlives the call",
  },
  {
    name: "playwright",
    pkg: "@playwright/mcp",
    bootOnly: "every tool drives a browser that installs out of band and outlives the call",
  },
  {
    name: "lemonsqueezy",
    pkg: "@yawlabs/lemonsqueezy-mcp",
    bootOnly: "all 64 tools call the Lemon Squeezy API with an account key",
  },
  {
    name: "ctxlint",
    pkg: "@yawlabs/ctxlint",
    args: ["serve"],
    call: {
      tool: "ctxlint_token_report",
      // Reads and tokenizes a fixture directory the harness wrote: local
      // files, no network, no credential, and nothing modified. Preferred over
      // ctxlint_validate_path (also credential-free) because it walks the
      // tree and runs the tokenizer, so it exercises more of the runtime than
      // a single stat does.
      args: () => ({ projectPath: fixture }),
      // One context file, fixed bytes: nothing in the report can drift, so the
      // two arms must agree exactly.
      deterministic: true,
      expect: (text) => {
        let report;
        try {
          report = JSON.parse(text);
        } catch {
          return "ctxlint_token_report returned text that is not the JSON report";
        }
        const counted = (report.files ?? []).map((f) => f.path);
        if (!counted.includes(FIXTURE_CONTEXT_FILE)) {
          return `ctxlint_token_report missed the fixture ${FIXTURE_CONTEXT_FILE} (counted ${JSON.stringify(counted)})`;
        }
        return report.totalTokens > 0
          ? null
          : "ctxlint_token_report counted zero tokens in a non-empty file";
      },
    },
  },
];

const BOOT_TIMEOUT_MS = 90_000;
const CALL_TIMEOUT_MS = 60_000;
const INSTALL_TIMEOUT_MS = 300_000;
// Distinctive on purpose: the fetch assertion looks for it in the body the
// sidecar hands back, which is the part that proves a real round trip rather
// than a status line the sidecar could have produced without one.
const LOOPBACK_MARKER = "oam-sidecar-matrix-fixture";

const argv = process.argv.slice(2);
const only = (argv.find((a) => a.startsWith("--only=")) ?? "").slice(7);
const selected = only
  ? SIDECARS.filter((s) => only.split(",").includes(s.name))
  : SIDECARS;

if (argv.includes("--list")) {
  for (const s of SIDECARS) {
    const what = s.call ? `calls ${s.call.tool}` : `boot only: ${s.bootOnly}`;
    console.log(`${s.name.padEnd(12)} ${s.pkg.padEnd(40)} ${what}`);
  }
  process.exit(0);
}
// Before anything that touches the disk, the network, or oam -- the self-test
// exists precisely so it can run where none of those are available.
if (argv.includes("--self-test")) process.exit(selfTest());
if (selected.length === 0) {
  console.error(`no sidecar matches --only=${only}; try --list`);
  process.exit(2);
}

const oamBin = process.env.OAM_BIN ?? "oam";

/** `npm install --no-save <specs...>` into the shared stage. */
function npmInstall(specs) {
  const r = spawnSync(
    process.platform === "win32" ? "npm.cmd" : "npm",
    ["install", "--no-save", "--no-audit", "--no-fund", "--prefix", stage, ...specs],
    { encoding: "utf8", timeout: INSTALL_TIMEOUT_MS, shell: process.platform === "win32" },
  );
  if (r.status === 0) return null;
  // npm's LAST stderr line is always "A complete log of this run can be found
  // in: ...", so taking the tail reported a log path instead of the reason and
  // made every SKIP undiagnosable. The first `npm error` line carries the code
  // (E404, EACCES) and the one after it the human sentence.
  const lines = (r.stderr || "")
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !/A complete log of this run/.test(l));
  const why = lines.find((l) => /^npm (error|ERR!)/.test(l)) ?? lines[0] ?? "?";
  return `npm install failed: ${why}`;
}

/** Install every selected package in ONE npm call.
 *
 *  One call, not one per sidecar, because `--no-save` leaves nothing declared
 *  in the stage: the next install sees the previous package as extraneous and
 *  PRUNES it. Per-sidecar installs therefore re-downloaded and re-extracted
 *  every tree on every run and left an empty stage behind. Installing them
 *  together makes them peers in one tree, so nothing prunes anything.
 *
 *  `@latest` stays explicit on every spec: this gate exists to catch a sidecar
 *  that PUBLISHED a break, so it must resolve the newest version each run
 *  rather than reuse whatever the stage happens to hold.
 *
 *  `install` is a seam, not configuration: production always passes nothing and
 *  gets npm. `--self-test` substitutes a recorder so the call SEQUENCE below --
 *  the part that has already been wrong once -- can be asserted offline. */
function installAll(pkgs, install = npmInstall) {
  const batch = install(pkgs.map((p) => `${p}@latest`));
  if (!batch) return new Map();
  // One bad package must not take the other six down with it. Install each on
  // its own to find out WHICH one npm rejected.
  process.stderr.write(`  batch install failed (${batch}); retrying one by one\n`);
  const failed = new Map();
  for (const pkg of pkgs) {
    const err = install([`${pkg}@latest`]);
    if (err) failed.set(pkg, err);
  }
  // Those solo installs pruned each other, so only the last one is still on
  // disk -- attribution is all they were for. Re-install the survivors
  // TOGETHER so they coexist for the probe loop; without this the fallback
  // would report every survivor as "not on disk after install".
  const survivors = pkgs.filter((p) => !failed.has(p));
  if (survivors.length > 0) {
    const err = install(survivors.map((p) => `${p}@latest`));
    if (err) for (const p of survivors) failed.set(p, err);
  }
  return failed;
}

// =============================================================================
// Adjudication -- whose fault is a failed tool call?
// =============================================================================
// A sidecar can publish a break at any time, and this gate runs against
// `@latest` precisely so it sees one. Without a control arm every such break
// reads as "oam broke it", the release is held for a bug oam does not have,
// and the next red run gets waved through on the assumption it is the same
// thing. So each invocation is adjudicated against the SAME call made on node.
//
// Pure, and separated from the process plumbing, because this is the judgement
// the gate's credibility rests on and it is the one part that can be asserted
// offline.

/** Adjudicate an oam tool-call verdict against the node control's.
 *  Verdicts are `{ ok: true, text }` or `{ ok: false, why }`.
 *  Returns `{ state, why?, note? }` where state is one of:
 *    verified -- oam answered and the answer holds
 *    fail     -- oam is at fault; this is the one state that reddens a release
 *    upstream -- the sidecar is broken on node too, so oam is not the suspect */
function classifyCall(oam, node, deterministic) {
  if (!oam.ok && !node.ok) {
    // The two arms often word a failure differently even when the cause is the
    // same -- an errno spelling, a path that is per-host by construction. That
    // is not enough to blame oam, so the verdict does not change; but claiming
    // they "fail the same way" when they plainly do not is the kind of line
    // that gets a gate distrusted, so the wording follows the evidence.
    const same = oam.why === node.why;
    return {
      state: "upstream",
      why: same
        ? `${oam.why}; node fails identically -- broken sidecar, not oam`
        : `${oam.why}; node also fails, differently (${node.why}) -- broken sidecar, not oam, though the two runtimes word it differently`,
    };
  }
  if (!oam.ok) {
    return { state: "fail", why: `${oam.why}; the node control PASSED, so this is oam` };
  }
  // oam answered and the control did not. That impugns the control, not the
  // runtime under test, so it must not redden the release -- but it is said
  // out loud, because an assertion only one arm can satisfy is on its way to
  // being worthless.
  if (!node.ok) {
    return { state: "verified", note: `node control failed (${node.why}); oam passed` };
  }
  if (deterministic && oam.text !== node.text) {
    return {
      state: "fail",
      why: `output differs from the node control: ${firstDifference(node.text, oam.text)}`,
    };
  }
  return { state: "verified" };
}

/** The first place two outputs diverge, with a little context on each side.
 *  A bare "outputs differ" on a multi-kilobyte tool result is unactionable. */
function firstDifference(expected, actual) {
  let i = 0;
  while (i < expected.length && i < actual.length && expected[i] === actual[i]) i += 1;
  const from = Math.max(0, i - 20);
  const show = (s) => JSON.stringify(s.slice(from, i + 40));
  return `at offset ${i}, node ${show(expected)} vs oam ${show(actual)}`;
}

// =============================================================================
// --self-test -- offline assertions about this harness's own decisions
// =============================================================================
// The install fallback is the one part of this harness that has already been
// wrong in production. It ended at the attribution loop, and because those solo
// `--no-save` installs prune each other, every survivor was then absent from
// the stage: resolveBin reported "not on disk after install" and the run
// degraded to an all-SKIP matrix that answered nothing about oam. One bad
// package silently disabled the gate.
//
// That is invisible in the happy path -- it only surfaces when a package really
// fails to install, which is exactly the run nobody is watching closely. So the
// SEQUENCE is what gets asserted, with a recorder standing in for npm: no
// network, no npm, no oam, no sidecars, so it can run anywhere and always.
//
// classifyCall is here for the same reason. Its two failure states are told
// apart only by the control arm, and both of them are rare by construction:
// nothing in a green run exercises the branch that decides whether a release
// gets held. The sidecar table is asserted too, so a sidecar added without a
// tool call and without a stated reason fails offline instead of silently
// widening the boot-only column.

/** Compares by JSON shape -- the assertions here are all arrays of specs, and a
 *  printed expected-vs-actual is what makes a regression diagnosable. */
function assertDeep(actual, expected, what) {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new Error(`${what}\nexpected: ${b}\nactual:   ${a}`);
}

/** Stands in for npmInstall: records every spec list it is handed, and returns
 *  npmInstall's own contract (an error string, or null on success).
 *  `rejects` names packages npm refuses; `rejectCall` fails the Nth call
 *  regardless, for conflicts that only exist when packages are installed
 *  together. */
function recordingInstaller({ rejects = [], rejectCall = null } = {}) {
  const calls = [];
  const install = (specs) => {
    calls.push([...specs]);
    if (rejectCall === calls.length) {
      return "npm install failed: npm error ERESOLVE could not resolve";
    }
    const bad = specs.find((s) => rejects.includes(s.replace(/@latest$/, "")));
    return bad ? `npm install failed: npm error E404 ${bad}` : null;
  };
  return { calls, install };
}

/** Runs the self-test cases. Returns the process exit code. */
function selfTest() {
  // Scoped and unscoped names both appear on purpose: the specs are built by
  // string concatenation, and `@scope/pkg@latest` is where that goes wrong.
  const cases = [
    {
      name: "one bad package: batch, then attribution, then a SURVIVOR RE-BATCH",
      run() {
        const pkgs = ["@scope/alpha", "bravo", "@scope/charlie"];
        const npm = recordingInstaller({ rejects: ["bravo"] });

        const failed = installAll(pkgs, npm.install);

        assertDeep(
          [...failed.keys()],
          ["bravo"],
          "only the package npm actually rejected is reported failed",
        );
        assertDeep(
          npm.calls[0],
          ["@scope/alpha@latest", "bravo@latest", "@scope/charlie@latest"],
          "first call installs every package in ONE batch",
        );
        assertDeep(
          npm.calls.slice(1, 4),
          [["@scope/alpha@latest"], ["bravo@latest"], ["@scope/charlie@latest"]],
          "a failed batch is attributed one package at a time",
        );
        // THE REGRESSION GUARD. Drop the survivor re-batch and this is the
        // assertion that fires: the last thing npm saw was a solo install,
        // which leaves only that one package on disk and every other survivor
        // resolving as "not on disk after install".
        assertDeep(
          npm.calls.at(-1),
          ["@scope/alpha@latest", "@scope/charlie@latest"],
          "the LAST call re-installs the survivors TOGETHER, without the failed package",
        );
        assertDeep(
          npm.calls.length,
          5,
          "batch + 3 attributions + survivor re-batch, and nothing else",
        );
      },
    },
    {
      name: "all packages install: exactly ONE npm call, no failures",
      run() {
        const pkgs = ["@scope/alpha", "bravo"];
        const npm = recordingInstaller();

        const failed = installAll(pkgs, npm.install);

        assertDeep([...failed.keys()], [], "a clean batch reports no failures");
        // The fallback is expensive (a full re-download per package). A green
        // run must not pay for it.
        assertDeep(
          npm.calls,
          [["@scope/alpha@latest", "bravo@latest"]],
          "a clean batch costs exactly one npm call -- no attribution, no re-batch",
        );
      },
    },
    {
      name: "every package fails: no trailing empty install",
      run() {
        const pkgs = ["alpha", "bravo"];
        const npm = recordingInstaller({ rejects: pkgs });

        const failed = installAll(pkgs, npm.install);

        assertDeep([...failed.keys()], pkgs, "every package is reported failed");
        // With no survivors there is nothing to re-batch, and a spec-less
        // `npm install --no-save --prefix <stage>` is not a harmless no-op --
        // it re-resolves the tree over the network for no benefit.
        assertDeep(npm.calls.length, 3, "batch + 2 attributions, and no survivor re-batch");
      },
    },
    {
      name: "survivor re-batch fails: survivors are attributed, not silently dropped",
      run() {
        const pkgs = ["alpha", "bravo", "charlie"];
        // bravo is rejected on its own; the 5th call -- the survivor re-batch --
        // is rejected too, the shape of a conflict only visible when the
        // survivors coexist.
        const npm = recordingInstaller({ rejects: ["bravo"], rejectCall: 5 });

        const failed = installAll(pkgs, npm.install);

        // Survivors that are NOT on disk must be reported, not left to fail
        // later as an undiagnosable SKIP.
        assertDeep(
          [...failed.keys()].sort(),
          ["alpha", "bravo", "charlie"],
          "a failed re-batch marks every survivor failed",
        );
        assertDeep(
          /ERESOLVE/.test(failed.get("alpha")),
          true,
          "a survivor carries the re-batch error, not the other package's E404",
        );
      },
    },
    {
      name: "a tool call that fails ONLY on oam is blamed on oam",
      run() {
        const v = classifyCall(
          { ok: false, why: "tool reported an error: ReferenceError: Buffer is not defined" },
          { ok: true, text: "fine" },
          false,
        );
        assertDeep(v.state, "fail", "oam broke a call node answers -- the release must go red");
        assertDeep(
          /node control PASSED/.test(v.why),
          true,
          "the reason says the control passed, so nobody re-litigates whose fault it is",
        );
      },
    },
    {
      name: "a tool call that fails on BOTH is blamed on the sidecar, never on oam",
      run() {
        const v = classifyCall(
          { ok: false, why: "tool reported an error: boom" },
          { ok: false, why: "tool reported an error: boom" },
          false,
        );
        // THE REASON THE CONTROL ARM EXISTS. Without it this is a red release
        // for a break oam did not cause, and the run after it gets waved
        // through as "that one again".
        assertDeep(v.state, "upstream", "a sidecar broken on node too is not an oam regression");
        assertDeep(/fails identically/.test(v.why), true, "and the wording says the two matched");

        const worded = classifyCall(
          { ok: false, why: "tool reported an error: EACCES" },
          { ok: false, why: "tool reported an error: EISDIR" },
          false,
        );
        assertDeep(worded.state, "upstream", "differing wording is still not an oam regression");
        assertDeep(
          /also fails, differently/.test(worded.why),
          true,
          "but the gate does not claim two different errors are the same error",
        );
      },
    },
    {
      name: "both arms answer: verified, and no note to explain away",
      run() {
        const v = classifyCall({ ok: true, text: "same" }, { ok: true, text: "same" }, true);
        assertDeep(v, { state: "verified" }, "matching answers are simply verified");
      },
    },
    {
      name: "deterministic output that diverges from node is an oam divergence",
      run() {
        const v = classifyCall(
          { ok: true, text: "tokens: 9" },
          { ok: true, text: "tokens: 8" },
          true,
        );
        assertDeep(v.state, "fail", "a differing answer is still a wrong answer");
        assertDeep(
          /offset 8/.test(v.why),
          true,
          "the reason points at the offset that differs, not just 'outputs differ'",
        );
        // Same texts, but the tool is one whose output legitimately moves
        // (a timestamp, a measured duration): comparing those byte for byte
        // would redden the gate on nothing at all.
        assertDeep(
          classifyCall({ ok: true, text: "9ms" }, { ok: true, text: "8ms" }, false).state,
          "verified",
          "a tool declared non-deterministic is judged on its assertion alone",
        );
      },
    },
    {
      name: "a failing node control does not redden a release oam passed",
      run() {
        const v = classifyCall(
          { ok: true, text: "answered" },
          { ok: false, why: "exited early (code 1)" },
          false,
        );
        assertDeep(v.state, "verified", "oam answered; the control is what is broken");
        assertDeep(
          /node control failed/.test(v.note ?? ""),
          true,
          "and it is said out loud rather than swallowed",
        );
      },
    },
    {
      name: "every sidecar declares either a tool call or a reason it cannot be invoked",
      run() {
        // The gate's coverage claim is exactly this table. A sidecar added
        // with neither field would boot, serve tools, print PASS, and be
        // counted in a summary that says nothing about it -- which is the
        // silent under-coverage this whole stage exists to end.
        const undecided = SIDECARS.filter((s) => !s.call === !s.bootOnly).map((s) => s.name);
        assertDeep(undecided, [], "no sidecar is left undecided (and none declares both)");
        const incomplete = SIDECARS.filter(
          (s) => s.call && !(s.call.tool && s.call.args && s.call.expect),
        ).map((s) => s.name);
        assertDeep(incomplete, [], "a declared call names a tool, its arguments, and an assertion");
      },
    },
  ];

  console.error("mcp-sidecar-matrix --self-test (offline)\n");
  let failures = 0;
  for (const c of cases) {
    try {
      c.run();
      console.error(`  PASS  ${c.name}`);
    } catch (e) {
      failures += 1;
      console.error(`  FAIL  ${c.name}`);
      console.error(e.message.replace(/^/gm, "          "));
    }
  }
  console.error(`\n${cases.length - failures}/${cases.length} self-tests passed`);
  if (failures > 0) console.error("the harness itself is broken -- fix it before trusting a run");
  return failures === 0 ? 0 : 1;
}

/** Resolve an installed package's bin entry point.
 *  Mirrors oam-spawn.ts: the BIN from package.json, not require.resolve --
 *  a package's library export is often ESM-gated and is not what npx runs. */
function resolveBin(pkg) {
  const dir = join(stage, "node_modules", ...pkg.split("/"));
  const manifestPath = join(dir, "package.json");
  if (!existsSync(manifestPath)) return { error: `not on disk after install: ${dir}` };
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const bin = manifest.bin;
  const rel = typeof bin === "string" ? bin : bin && Object.values(bin)[0];
  if (!rel) return { error: "package.json declares no bin" };
  const entry = resolve(dir, rel);
  if (!existsSync(entry)) return { error: `bin missing on disk: ${entry}` };
  return { entry };
}

/** Turn a tools/call reply into a verdict the adjudicator can judge.
 *  A tool that answers with `isError` is a FAILED call: the protocol carried it
 *  fine, which is exactly the shape a sidecar-that-only-lists-tools has. */
function callVerdict(msg, expect) {
  if (msg.error) return { ok: false, why: `tools/call error: ${msg.error.message}` };
  const result = msg.result;
  if (!result) return { ok: false, why: "tools/call returned neither result nor error" };
  const blocks = Array.isArray(result.content) ? result.content : [];
  const text = blocks
    .filter((b) => b?.type === "text" && typeof b.text === "string")
    .map((b) => b.text)
    .join("\n");
  if (result.isError) {
    return { ok: false, why: `tool reported an error: ${(text || "(no text)").split("\n")[0]}` };
  }
  if (blocks.length === 0) return { ok: false, why: "result carried no content blocks" };
  if (!text) return { ok: false, why: "result carried no text content" };
  const bad = expect(text);
  return bad ? { ok: false, why: bad } : { ok: true, text };
}

/** Speak MCP over stdio to a sidecar hosted on `host` ("oam" or "node").
 *  Resolves with the tool names it serves and, when `call` is set, the verdict
 *  on invoking that tool. */
function probe(host, entry, { env = {}, scriptArgs = [], call = null, loopbackUrl = null } = {}) {
  // `--` is REQUIRED before script args: `oam run` declares script_args with
  // clap's `last = true`, so `oam run entry.js serve` is "unexpected argument".
  // This mirrors oam-spawn.ts exactly (`["run", entry, "--", ...rest]` when
  // rest is non-empty, a bare `["run", entry]` when it is not) -- a harness
  // that always appended `--` would still pass while production differs.
  //
  // The node arm is node's own plain invocation, which is what the broker falls
  // back to and therefore the right reference: `node <entry> [...rest]`.
  const cmd = host === "oam" ? oamBin : process.execPath;
  const oamArgv = scriptArgs.length > 0 ? ["run", entry, "--", ...scriptArgs] : ["run", entry];
  const argv = host === "oam" ? oamArgv : [entry, ...scriptArgs];
  const ctx = { host, fixture, loopbackUrl };
  const callEnv = call?.env ? call.env(ctx) : {};
  return new Promise((resolveP) => {
    const child = spawn(cmd, argv, {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, NO_COLOR: "1", ...env, ...callEnv },
    });

    let out = "";
    let stderr = "";
    let settled = false;
    // Which reply the harness is waiting for, so a hang names the phase it hung
    // in: "boots but never answers a tool call" and "never boots" are different
    // bugs and used to print the same line.
    let awaiting = "tools/list";
    let tools = [];
    let timer = null;
    const done = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.kill();
      resolveP(result);
    };
    const wait = (what, ms) => {
      awaiting = what;
      clearTimeout(timer);
      timer = setTimeout(
        () => done({ ok: false, why: `no ${awaiting} response within ${ms / 1000}s`, stderr }),
        ms,
      );
    };
    wait("tools/list", BOOT_TIMEOUT_MS);

    child.on("error", (e) => done({ ok: false, why: `spawn failed: ${e.message}`, stderr }));
    child.on("exit", (code) =>
      done({ ok: false, why: `exited early (code ${code}) awaiting ${awaiting}`, stderr }),
    );
    child.stderr.on("data", (d) => {
      stderr += d;
    });

    const send = (msg) => child.stdin.write(`${JSON.stringify(msg)}\n`);

    child.stdout.on("data", (d) => {
      out += d;
      // Newline-delimited JSON-RPC; a partial trailing line is kept for the
      // next chunk rather than parsed and discarded.
      const lines = out.split("\n");
      out = lines.pop() ?? "";
      for (const line of lines) {
        if (!line.trim()) continue;
        let msg;
        try {
          msg = JSON.parse(line);
        } catch {
          continue; // sidecars sometimes log non-JSON to stdout
        }
        if (msg.id === 1 && msg.result) {
          send({ jsonrpc: "2.0", method: "notifications/initialized" });
          send({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
        } else if (msg.id === 2) {
          if (msg.error) {
            done({ ok: false, why: `tools/list error: ${msg.error.message}`, stderr });
            return;
          }
          tools = (msg.result?.tools ?? []).map((t) => t.name);
          if (tools.length === 0) {
            done({ ok: false, why: "served an EMPTY tool list", stderr });
            return;
          }
          if (!call) {
            done({ ok: true, tools, stderr });
            return;
          }
          // The tool has to be one the sidecar just said it has. Calling a
          // renamed tool anyway gets "unknown tool" back from BOTH arms, which
          // adjudicates as a broken sidecar and buries the actual news: this
          // harness is asserting against something that no longer exists and
          // needs a new tool picked. Same verdict (not oam), better sentence.
          if (!tools.includes(call.tool)) {
            done({
              ok: true,
              tools,
              stale: true,
              call: {
                ok: false,
                why: `no longer advertises ${call.tool} (it serves ${tools.length} other tools) -- pick a new tool for the matrix`,
              },
              stderr,
            });
            return;
          }
          wait(`tools/call ${call.tool}`, CALL_TIMEOUT_MS);
          send({
            jsonrpc: "2.0",
            id: 3,
            method: "tools/call",
            params: { name: call.tool, arguments: call.args(ctx) },
          });
        } else if (msg.id === 3) {
          done({ ok: true, tools, call: callVerdict(msg, call.expect), stderr });
          return;
        }
      }
    });

    send({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2024-11-05",
        capabilities: {},
        clientInfo: { name: "oam-sidecar-matrix", version: "1" },
      },
    });
  });
}

/** The first line of a sidecar's stderr that looks like a diagnosis.
 *  Same tail-picking trap npmInstall had: an oam fatal report ends with the
 *  `oam v0.8.3` version footer, so `.pop()` showed the operator a version
 *  string instead of the error. This is often the only clue the gate emits
 *  when a sidecar breaks, so lead with the first line that reads like one. */
function diagnosis(stderr) {
  const lines = (stderr || "")
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !/^oam v\d/.test(l));
  return (
    lines.find((l) => /^([A-Za-z]*Error|Uncaught|panicked|oam:|npm error)/.test(l)) ?? lines[0] ?? ""
  );
}

/** Write the tool-call fixture tree. Wiped first, so a run never inherits
 *  whatever the last one -- or a sidecar under test -- left behind. */
function prepareFixture() {
  rmSync(fixture, { recursive: true, force: true });
  mkdirSync(fixture, { recursive: true });
  writeFileSync(join(fixture, FIXTURE_CONTEXT_FILE), FIXTURE_CONTEXT_BODY);
}

/** A loopback HTTP server for the sidecars whose every tool is an HTTP call.
 *  Resolves to a URL, or to null with the reason it could not listen -- on a
 *  box that forbids the bind, "fetch was not invoked and here is why" is the
 *  honest result, and a great deal better than a red release. */
function startLoopback() {
  return new Promise((resolveP) => {
    const server = createServer((_req, res) => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ fixture: LOOPBACK_MARKER }));
    });
    server.once("error", (e) => resolveP({ error: `loopback fixture server: ${e.message}` }));
    server.listen(0, "127.0.0.1", () => {
      resolveP({ server, url: `http://127.0.0.1:${server.address().port}/` });
    });
  });
}

const version = spawnSync(oamBin, ["--version"], { encoding: "utf8" });
if (version.status !== 0) {
  console.error(`cannot run '${oamBin}' -- set OAM_BIN or put oam on PATH`);
  process.exit(2);
}
console.error(`oam sidecar matrix -- ${version.stdout.trim()}`);
console.error(`stage: ${stage}\n`);

mkdirSync(stage, { recursive: true });
prepareFixture();

process.stderr.write(`  installing ${selected.length} package(s)...`);
const installErrors = installAll(selected.map((s) => s.pkg));
process.stderr.write(`\r${"".padEnd(40)}\r`);

const loopback = selected.some((s) => s.call?.needsLoopback)
  ? await startLoopback()
  : { url: null };

// One column layout for every line the run emits, continuations included: a
// verdict that does not line up under the sidecar it belongs to is read as
// belonging to the next one.
const row = (name, banner, detail) => `  ${name.padEnd(12)} ${banner.padEnd(8)}  ${detail}\n`;
const under = (detail) => row("", "", detail);
const results = [];
for (const s of selected) {
  const { entry, error } = installErrors.has(s.pkg)
    ? { error: installErrors.get(s.pkg) }
    : resolveBin(s.pkg);
  if (error) {
    process.stderr.write(row(s.name, "SKIP", error));
    results.push({ name: s.name, state: "skip", why: error });
    continue;
  }
  // A sidecar whose call needs the loopback server cannot be invoked when the
  // bind failed. Demote it to boot-only WITH THE REASON rather than reporting
  // a failure oam had no part in.
  const call = s.call && !(s.call.needsLoopback && !loopback.url) ? s.call : null;
  const bootOnly = s.bootOnly ?? (s.call && !call ? loopback.error : null);

  process.stderr.write(`  ${s.name.padEnd(12)} probing...`);
  const shared = { env: s.env ?? {}, scriptArgs: s.args ?? [], loopbackUrl: loopback.url };
  const oam = await probe("oam", entry, { ...shared, call });
  const emit = (banner, detail, extra = []) => {
    // Clear the in-place progress text first: a short verdict written over a
    // longer "control..." leaves its tail behind and reads as part of the line.
    process.stderr.write(`\r${"".padEnd(30)}\r${row(s.name, banner, detail)}`);
    for (const line of extra) if (line) process.stderr.write(under(line.slice(0, 140)));
  };

  if (!oam.ok) {
    emit("FAIL", oam.why, [diagnosis(oam.stderr)]);
    results.push({ name: s.name, state: "fail", why: oam.why });
    continue;
  }
  if (!call) {
    emit("BOOT", `${oam.tools.length} tools, not invoked: ${bootOnly}`);
    results.push({ name: s.name, state: "boot", why: bootOnly });
    continue;
  }
  // A tool the sidecar no longer advertises is answered by neither runtime, so
  // there is nothing to adjudicate -- and running the control to confirm that
  // would only spend a spawn to reach the same sentence.
  if (oam.stale) {
    emit("UPSTREAM", oam.call.why);
    results.push({ name: s.name, state: "upstream", why: oam.call.why });
    continue;
  }

  // The control arm. Run for EVERY invocation, not only failing ones: it is
  // what decides whose bug a red is, and a control taken only after a failure
  // is how "oam broke it" gets asserted first and checked second.
  process.stderr.write(`\r  ${s.name.padEnd(12)} control...`);
  const control = await probe("node", entry, { ...shared, call });
  const nodeVerdict = control.ok
    ? (control.call ?? { ok: false, why: "node control returned no verdict" })
    : { ok: false, why: `node control could not probe: ${control.why}` };

  const verdict = classifyCall(oam.call, nodeVerdict, call.deterministic === true);
  const banner = { verified: "PASS", fail: "FAIL", upstream: "UPSTREAM" }[verdict.state];
  const detail =
    verdict.state === "verified"
      ? `${oam.tools.length} tools, ${call.tool} verified against node`
      : `${call.tool}: ${verdict.why}`;
  emit(banner, detail, [
    verdict.note ? `note: ${verdict.note}` : "",
    verdict.state === "verified" ? "" : diagnosis(oam.stderr) || diagnosis(control.stderr),
  ]);
  results.push({ name: s.name, state: verdict.state, why: verdict.why });
}

loopback.server?.close();

const count = (state) => results.filter((r) => r.state === state).length;
const named = (state) =>
  results
    .filter((r) => r.state === state)
    .map((r) => r.name)
    .join(", ");
const fail = results.filter((r) => r.state === "fail");
const upstream = results.filter((r) => r.state === "upstream");
const skip = results.filter((r) => r.state === "skip");

// The counts are split because collapsing them is the failure this stage was
// added to fix: "8/9 served tools" read as full coverage of a set where none
// of the eight had ever been asked to DO anything.
const parts = [`${count("verified")} tool-call verified`, `${count("boot")} boot only`];
if (upstream.length) parts.push(`${upstream.length} broken upstream`);
if (fail.length) parts.push(`${fail.length} FAILED on oam`);
if (skip.length) parts.push(`${skip.length} could not install`);
console.error(`\n${results.length} sidecars on oam: ${parts.join(", ")}`);
if (count("boot") > 0) {
  console.error(`  boot only (no credential-free, side-effect-free tool): ${named("boot")}`);
}

// Neither a skip nor an upstream break is a pass: both mean the matrix could
// not answer for that sidecar, and saying so is the difference between a gate
// and a rubber stamp. Boot-only is different in kind -- it is the gate's known
// and printed coverage, not a run that fell short -- so it does not hold a
// release, and pretending otherwise would make every run exit non-zero and
// teach the operator to ignore the number.
if (fail.length > 0) {
  console.error(`FAILED on oam: ${fail.map((f) => f.name).join(", ")}`);
  process.exit(1);
}
if (upstream.length > 0 || skip.length > 0) {
  console.error("no oam failures, but the matrix is INCOMPLETE (see above)");
  process.exit(3);
}
process.exit(0);
