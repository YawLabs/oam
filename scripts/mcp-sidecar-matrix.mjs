#!/usr/bin/env node
// =============================================================================
// MCP sidecar regression matrix -- does each sidecar still BOOT and SERVE TOOLS
// when hosted on oam?
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
// Runs on NODE, deliberately: it is testing oam, and a harness hosted on the
// runtime under test turns "oam is broken" into "the harness is broken".
//
// Usage:
//   node scripts/mcp-sidecar-matrix.mjs                 # every oam-hosted sidecar
//   node scripts/mcp-sidecar-matrix.mjs --only=fetch,memory
//   node scripts/mcp-sidecar-matrix.mjs --list
//   node scripts/mcp-sidecar-matrix.mjs --self-test     # checks THIS harness;
//                                                       # no network, no npm, no oam
//   OAM_BIN=/path/to/oam node scripts/mcp-sidecar-matrix.mjs
//
// Exit code is the gate: 0 only when every selected sidecar served tools.
// =============================================================================

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";

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
const SIDECARS = [
  { name: "memory", pkg: "@modelcontextprotocol/server-memory" },
  { name: "fetch", pkg: "@yawlabs/fetch-mcp" },
  { name: "tailscale", pkg: "@yawlabs/tailscale-mcp" },
  { name: "postgres", pkg: "@yawlabs/postgres-mcp" },
  {
    name: "redis",
    pkg: "@yawlabs/redis-mcp",
    env: { REDIS_URL: "redis://127.0.0.1:6379" },
  },
  { name: "puppeteer", pkg: "@modelcontextprotocol/server-puppeteer" },
  { name: "playwright", pkg: "@playwright/mcp" },
  { name: "lemonsqueezy", pkg: "@yawlabs/lemonsqueezy-mcp" },
  { name: "ctxlint", pkg: "@yawlabs/ctxlint", args: ["serve"] },
];

const BOOT_TIMEOUT_MS = 90_000;
const INSTALL_TIMEOUT_MS = 300_000;

const argv = process.argv.slice(2);
const only = (argv.find((a) => a.startsWith("--only=")) ?? "").slice(7);
const selected = only
  ? SIDECARS.filter((s) => only.split(",").includes(s.name))
  : SIDECARS;

if (argv.includes("--list")) {
  for (const s of SIDECARS) console.log(`${s.name.padEnd(12)} ${s.pkg}`);
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
const stage = join(tmpdir(), "oam-mcp-matrix");
mkdirSync(stage, { recursive: true });

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
// --self-test -- offline assertions about installAll's own call sequence
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

/** Speak MCP over stdio to a sidecar hosted on oam. Resolves with the tool
 *  names it serves, or rejects with why it could not. */
function probe(entry, extraEnv = {}, scriptArgs = []) {
  // `--` is REQUIRED before script args: `oam run` declares script_args with
  // clap's `last = true`, so `oam run entry.js serve` is "unexpected argument".
  // This mirrors oam-spawn.ts exactly (`["run", entry, "--", ...rest]` when
  // rest is non-empty, a bare `["run", entry]` when it is not) -- a harness
  // that always appended `--` would still pass while production differs.
  const argv = scriptArgs.length > 0 ? ["run", entry, "--", ...scriptArgs] : ["run", entry];
  return new Promise((resolveP) => {
    const child = spawn(oamBin, argv, {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, NO_COLOR: "1", ...extraEnv },
    });

    let out = "";
    let stderr = "";
    let settled = false;
    const done = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      child.kill();
      resolveP(result);
    };

    const timer = setTimeout(
      () =>
        done({
          ok: false,
          why: `no tools/list response within ${BOOT_TIMEOUT_MS / 1000}s`,
          stderr,
        }),
      BOOT_TIMEOUT_MS,
    );

    child.on("error", (e) => done({ ok: false, why: `spawn failed: ${e.message}`, stderr }));
    child.on("exit", (code) =>
      done({ ok: false, why: `exited early (code ${code})`, stderr }),
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
          } else {
            const tools = (msg.result?.tools ?? []).map((t) => t.name);
            done(
              tools.length > 0
                ? { ok: true, tools }
                : { ok: false, why: "served an EMPTY tool list", stderr },
            );
          }
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

const version = spawnSync(oamBin, ["--version"], { encoding: "utf8" });
if (version.status !== 0) {
  console.error(`cannot run '${oamBin}' -- set OAM_BIN or put oam on PATH`);
  process.exit(2);
}
console.error(`oam sidecar matrix -- ${version.stdout.trim()}`);
console.error(`stage: ${stage}\n`);

process.stderr.write(`  installing ${selected.length} package(s)...`);
const installErrors = installAll(selected.map((s) => s.pkg));
process.stderr.write(`\r${"".padEnd(40)}\r`);

const results = [];
for (const s of selected) {
  const { entry, error } = installErrors.has(s.pkg)
    ? { error: installErrors.get(s.pkg) }
    : resolveBin(s.pkg);
  if (error) {
    process.stderr.write(`  ${s.name.padEnd(12)} SKIP  ${error}\n`);
    results.push({ ...s, state: "skip", why: error });
    continue;
  }
  process.stderr.write(`  ${s.name.padEnd(12)} probing... `);
  const r = await probe(entry, s.env ?? {}, s.args ?? []);
  if (r.ok) {
    process.stderr.write(`\r  ${s.name.padEnd(12)} PASS  ${r.tools.length} tools\n`);
    results.push({ ...s, state: "pass", tools: r.tools.length });
  } else {
    // Same tail-picking trap npmInstall had: an oam fatal report ends with the
    // `oam v0.8.3` version footer, so `.pop()` showed the operator a version
    // string instead of the error. This is the only clue the gate emits when a
    // sidecar breaks, so lead with the first line that looks like a diagnosis.
    const lines = (r.stderr || "")
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l && !/^oam v\d/.test(l));
    const tail =
      lines.find((l) => /^([A-Za-z]*Error|Uncaught|panicked|oam:|npm error)/.test(l)) ??
      lines[0] ??
      "";
    process.stderr.write(`\r  ${s.name.padEnd(12)} FAIL  ${r.why}\n`);
    if (tail) process.stderr.write(`  ${"".padEnd(12)}       ${tail.slice(0, 120)}\n`);
    results.push({ ...s, state: "fail", why: r.why });
  }
}

const pass = results.filter((r) => r.state === "pass").length;
const fail = results.filter((r) => r.state === "fail");
const skip = results.filter((r) => r.state === "skip");
console.error(
  `\n${pass}/${results.length} sidecars served tools on oam` +
    (skip.length ? `  (${skip.length} skipped: could not install)` : ""),
);
// A skip is NOT a pass: it means the matrix could not answer for that sidecar,
// and saying so is the difference between a gate and a rubber stamp.
if (fail.length > 0) {
  console.error(`FAILED: ${fail.map((f) => f.name).join(", ")}`);
  process.exit(1);
}
if (skip.length > 0) {
  console.error("no failures, but the matrix is INCOMPLETE (see skips above)");
  process.exit(3);
}
process.exit(0);
