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
//   OAM_BIN=/path/to/oam node scripts/mcp-sidecar-matrix.mjs
//
// Exit code is the gate: 0 only when every selected sidecar served tools.
// =============================================================================

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";

// The oam-hosted set from bundles.json. `github` is docker-hosted and never
// rewritten; lemonsqueezy and ctxlint have not been opted in.
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
if (selected.length === 0) {
  console.error(`no sidecar matches --only=${only}; try --list`);
  process.exit(2);
}

const oamBin = process.env.OAM_BIN ?? "oam";
const stage = join(tmpdir(), "oam-mcp-matrix");
mkdirSync(stage, { recursive: true });

/** Install `pkg` into the shared stage and return its resolved bin entry.
 *  Mirrors oam-spawn.ts: the BIN from package.json, not require.resolve --
 *  a package's library export is often ESM-gated and is not what npx runs. */
function installAndResolve(pkg) {
  const r = spawnSync(
    process.platform === "win32" ? "npm.cmd" : "npm",
    ["install", "--no-save", "--no-audit", "--no-fund", "--prefix", stage, `${pkg}@latest`],
    { encoding: "utf8", timeout: INSTALL_TIMEOUT_MS, shell: process.platform === "win32" },
  );
  if (r.status !== 0) {
    return { error: `npm install failed: ${(r.stderr || "").trim().split("\n").pop() ?? "?"}` };
  }
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
function probe(entry, extraEnv = {}) {
  return new Promise((resolveP) => {
    const child = spawn(oamBin, ["run", entry], {
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

const results = [];
for (const s of selected) {
  process.stderr.write(`  ${s.name.padEnd(12)} installing...`);
  const { entry, error } = installAndResolve(s.pkg);
  if (error) {
    process.stderr.write(`\r  ${s.name.padEnd(12)} SKIP  ${error}\n`);
    results.push({ ...s, state: "skip", why: error });
    continue;
  }
  process.stderr.write(`\r  ${s.name.padEnd(12)} probing... `);
  const r = await probe(entry, s.env ?? {});
  if (r.ok) {
    process.stderr.write(`\r  ${s.name.padEnd(12)} PASS  ${r.tools.length} tools\n`);
    results.push({ ...s, state: "pass", tools: r.tools.length });
  } else {
    const tail = (r.stderr || "").trim().split("\n").pop() ?? "";
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
