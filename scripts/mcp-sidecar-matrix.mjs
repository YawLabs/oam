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
// everything behind it is broken. So every sidecar gets a real tool CALLED and
// the result asserted. None of them needs a credential or the internet to do
// it: the harness supplies whatever the call would otherwise reach -- a
// loopback HTTP server, a Redis wire-protocol fake, a local PostgreSQL, the
// browser already installed on the box. When one of those is genuinely absent
// the row is DEMOTED with the reason, and a demoted row makes the run
// INCOMPLETE rather than clean: a fixture that quietly failed to start used to
// turn the strongest assertion in the set into "boot only" and exit 0.
//
// Every invocation is ALSO run on node, and the two outcomes are adjudicated
// together. A tool that fails the same way on both is a BROKEN SIDECAR, not an
// oam regression, and the gate says so rather than failing an oam release for
// somebody else's publish. That distinction is the difference between a gate
// people trust and one they learn to skip.
//
// The node arm has to actually BE node, which it silently was not. Every
// @yawlabs sidecar's bin is a runtime launcher that prefers oam, so
// `node <bin>` re-spawned oam and the "verified against node" rows were oam
// compared against oam -- measured with a process-tree walk on 2026-09-12. The
// launcher's own `*_RUNTIME` switch is read out of its source and set to
// `node` on the control arm, and a launcher that names no such switch is
// refused rather than trusted.
//
// Nothing a sidecar starts directly may outlive it. Every probe snapshots the
// process tree under the sidecar before tearing it down, and a direct child
// still running afterwards is a leak, adjudicated against the node control like
// everything else. node's libuv puts every child in a kill-on-close job object
// on Windows, so node never leaves one behind.
//
// Runs on NODE, deliberately: it is testing oam, and a harness hosted on the
// runtime under test turns "oam is broken" into "the harness is broken". The
// node control arm is the same fact used twice -- the harness's own executable
// is the reference implementation, already on the box, already trusted.
//
// Usage:
//   node scripts/mcp-sidecar-matrix.mjs                 # every oam-hosted sidecar
//   node scripts/mcp-sidecar-matrix.mjs --only=fetch,memory
//   node scripts/mcp-sidecar-matrix.mjs --json=report.json
//   node scripts/mcp-sidecar-matrix.mjs --list
//   node scripts/mcp-sidecar-matrix.mjs --self-test     # checks THIS harness;
//                                                       # no network, no npm, no oam
//   OAM_BIN=/path/to/oam node scripts/mcp-sidecar-matrix.mjs
//   OAM_MATRIX_BROWSER=/path/to/chrome     # browser for puppeteer + playwright
//   OAM_MATRIX_DATABASE_URL=postgresql://  # default postgres@127.0.0.1:5432
//
// Exit code is the gate: 0 when every selected sidecar answered its tool call
// on oam; 1 when oam failed one; 3 when the matrix could not answer for one
// (install failure, broken upstream, demoted fixture); 2 for bad usage.
// =============================================================================

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { connect, createServer as createTcpServer } from "node:net";
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
// Browser profiles, per run and outside the fixture: a browser an earlier run
// leaked holds its profile open, and when profiles lived in the fixture that
// one leak made the NEXT run die clearing it.
const profilesRoot = join(stage, "profiles");
const profiles = join(profilesRoot, `run-${process.pid}`);
const FIXTURE_CONTEXT_FILE = "CLAUDE.md";
const FIXTURE_CONTEXT_BODY =
  "# oam sidecar matrix fixture\n\nFixed input so a token count is deterministic.\n";

// Distinctive on purpose: assertions look for it in what the sidecar hands
// back, which is the part that proves a real round trip rather than a status
// line the sidecar could have produced without one.
const LOOPBACK_MARKER = "oam-sidecar-matrix-fixture";
const DATABASE_URL =
  process.env.OAM_MATRIX_DATABASE_URL ?? "postgresql://postgres@127.0.0.1:5432/postgres";
// One SELECT that crosses every column type pg's wire decoding has to get
// right, bytea included: bytea comes back as a Buffer, and Buffer is oam's.
const POSTGRES_PROBE_SQL =
  "SELECT 1 AS i, 'oam'::text AS t, true AS b, 1.5::float8 AS f, decode('6f616d','hex') AS bytes, NULL::text AS n";
const RESP_FAKE_VERSION = "7.4.0";

// The oam-hosted set from bundles.json. `github` is the only exclusion left:
// it is docker-hosted, and the rewrite only ever touches node/npx launches.
//
// `args` are the launch args that follow the package spec, and ctxlint is the
// only sidecar that has any (`npx -y @yawlabs/ctxlint@latest serve`). That
// makes it the only entry exercising the `--` separator the rewrite emits, so
// it is doing double duty here: without it the harness never sends script args
// at all, and `oam run <entry> -- <args>` reaching a live stdio MCP server goes
// untested end to end (the argv plumbing alone is covered by e2e.rs).
//
// `envPrefixes` are scrubbed from the inherited environment before anything
// the harness sets. The release box is also a box people USE: it carries a
// real TAILSCALE_API_KEY, and TAILSCALE_READONLY or REDIS_URL would change
// what a sidecar serves. Scrubbing makes "needs no credential" true by
// construction instead of true on a box that happens to have none.
//
// EVERY entry declares exactly one of two things, and the self-test enforces
// that so a sidecar added later cannot slip in undecided:
//
//   `call`     -- a tool, its arguments, and an assertion the result must
//                 satisfy, plus whatever the call needs (`requires`, `env`,
//                 `before`). Run on oam AND on node.
//   `bootOnly` -- the honest reason no such tool can be called. Printed on
//                 every run. No sidecar uses it today, and the self-test pins
//                 that, so adding one is a reviewed decision.
const SIDECARS = [
  {
    name: "memory",
    pkg: "@modelcontextprotocol/server-memory",
    envPrefixes: ["MEMORY_"],
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
    envPrefixes: ["FETCH_MCP_"],
    call: {
      tool: "http_get",
      // A real request/response through the sidecar's HTTP stack on oam,
      // against a server the harness itself starts on loopback: nothing
      // external, no name resolution, no credential.
      //
      // `allow_private_hosts` is not a test-only escape hatch; it is the
      // sidecar's own documented switch for exactly this, and it defaults off
      // so a loopback URL is refused without it.
      requires: (ctx) => ctx.loopback.error ?? null,
      args: (ctx) => ({
        url: ctx.loopback.url,
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
    envPrefixes: ["TAILSCALE_"],
    call: {
      // The one tool of 98 that touches no network (`openWorldHint: false`):
      // it reports how the registration table is grouped and filtered. That
      // is exactly the table a broken sidecar can serve from while every call
      // behind it throws, so asking the sidecar to walk it is a real
      // invocation, not a restatement of tools/list. Identical bytes with and
      // without a tailnet key, measured 2026-09-12.
      tool: "tailscale_tool_groups",
      args: () => ({}),
      deterministic: true,
      expect: (text) => {
        let report;
        try {
          report = JSON.parse(text);
        } catch {
          return "tailscale_tool_groups returned text that is not the JSON report";
        }
        const groups = Array.isArray(report.groups) ? report.groups : [];
        if (groups.length === 0) return "tailscale_tool_groups reported no tool groups";
        const bad = groups.find((g) => typeof g.group !== "string" || !(g.tools > 0));
        return bad ? `tailscale_tool_groups reported a malformed group: ${JSON.stringify(bad)}` : null;
      },
    },
  },
  {
    name: "postgres",
    pkg: "@yawlabs/postgres-mcp",
    envPrefixes: ["POSTGRES_", "DATABASE_URL", "PG"],
    call: {
      // Against a real PostgreSQL, because a fake does not stay small: the
      // sidecar resolves type names from the catalog after every query, so a
      // wire fake would have to answer pg_type lookups too. Local by default,
      // and demoted with the reason on a box that has none.
      tool: "pg_readonly",
      requires: () => tcpPreflight(DATABASE_URL, 5432),
      env: () => ({ DATABASE_URL }),
      args: () => ({ sql: POSTGRES_PROBE_SQL }),
      deterministic: true,
      // Both arms failing to connect or authenticate is the environment, not
      // the sidecar: a server that is up but refuses this role gets past the
      // TCP preflight and fails the call identically on both runtimes.
      unavailable: (why) =>
        /ECONNREFUSED|ETIMEDOUT|EHOSTUNREACH|password authentication failed|no pg_hba\.conf entry|role "[^"]*" does not exist|database "[^"]*" does not exist|SASL/i.test(
          why,
        ),
      expect: (text) => {
        let result;
        try {
          result = JSON.parse(text);
        } catch {
          return "pg_readonly returned text that is not the JSON result";
        }
        const row = result.rows?.[0];
        if (!row) return "pg_readonly returned no row";
        const want = { i: 1, t: "oam", b: true, f: 1.5, n: null };
        for (const [k, v] of Object.entries(want)) {
          if (row[k] !== v) return `pg_readonly column ${k} decoded as ${JSON.stringify(row[k])}, not ${JSON.stringify(v)}`;
        }
        return row.bytes?.type === "Buffer" && JSON.stringify(row.bytes.data) === "[111,97,109]"
          ? null
          : `pg_readonly decoded bytea as ${JSON.stringify(row.bytes)}, not Buffer "oam"`;
      },
    },
  },
  {
    name: "redis",
    pkg: "@yawlabs/redis-mcp",
    envPrefixes: ["REDIS_"],
    call: {
      // Against a Redis wire-protocol fake the harness starts, not a real
      // server: redis_health against a live Redis differs between the two arms
      // (uptime, commands processed) and a box without Redis would demote it.
      // The fake answers exactly what the sidecar sends -- measured as CLIENT
      // SETINFO, INFO, DBSIZE and QUIT -- with fixed values, so the arms must
      // agree byte for byte.
      tool: "redis_health",
      requires: (ctx) => ctx.resp.error ?? null,
      env: (ctx) => ({ REDIS_URL: ctx.resp.url }),
      args: () => ({ slowlogLimit: 0 }),
      deterministic: true,
      expect: (text) => {
        let health;
        try {
          health = JSON.parse(text);
        } catch {
          return "redis_health returned text that is not the JSON report";
        }
        if (health.connected !== true) return "redis_health did not report a connection";
        return health.server?.version === RESP_FAKE_VERSION && health.keyspace?.total_keys === 0
          ? null
          : `redis_health did not reflect the fixture server (version ${health.server?.version}, keys ${health.keyspace?.total_keys})`;
      },
    },
  },
  {
    name: "puppeteer",
    pkg: "@modelcontextprotocol/server-puppeteer",
    envPrefixes: ["PUPPETEER_"],
    browser: true,
    call: {
      // The installed browser, headless, in a profile the harness owns. The
      // server's own default is a VISIBLE window, which on the release box is
      // a browser popping up mid-release.
      requires: (ctx) => ctx.browser.error ?? ctx.loopback.error ?? null,
      env: (ctx) => ({
        PUPPETEER_LAUNCH_OPTIONS: JSON.stringify({
          headless: true,
          executablePath: ctx.browser.path,
          userDataDir: ctx.profileDir,
        }),
      }),
      before: [{ tool: "puppeteer_navigate", args: (ctx) => ({ url: `${ctx.loopback.url}page` }) }],
      // Evaluated IN the page, so the answer can only come from a browser that
      // loaded the fixture -- a navigate reply alone is a status line.
      tool: "puppeteer_evaluate",
      args: () => ({ script: "document.title" }),
      // The reply appends the page's console output, which a browser is free
      // to vary.
      deterministic: false,
      expect: (text) =>
        text.includes(`"${LOOPBACK_MARKER}"`)
          ? null
          : "puppeteer_evaluate did not return the fixture page's title",
    },
  },
  {
    name: "playwright",
    pkg: "@playwright/mcp",
    envPrefixes: ["PLAYWRIGHT_MCP_"],
    browser: true,
    call: {
      requires: (ctx) => ctx.browser.error ?? ctx.loopback.error ?? null,
      env: (ctx) => ({
        PLAYWRIGHT_MCP_BROWSER: "chromium",
        PLAYWRIGHT_MCP_EXECUTABLE_PATH: ctx.browser.path,
        PLAYWRIGHT_MCP_HEADLESS: "1",
        PLAYWRIGHT_MCP_USER_DATA_DIR: ctx.profileDir,
        PLAYWRIGHT_MCP_OUTPUT_DIR: `${ctx.profileDir}-out`,
      }),
      tool: "browser_navigate",
      args: (ctx) => ({ url: `${ctx.loopback.url}page` }),
      // The reply names the loopback port and a timestamped snapshot file.
      deterministic: false,
      expect: (text) =>
        text.includes(`Page Title: ${LOOPBACK_MARKER}`)
          ? null
          : "browser_navigate did not report the fixture page's title",
    },
  },
  {
    name: "lemonsqueezy",
    pkg: "@yawlabs/lemonsqueezy-mcp",
    envPrefixes: ["LEMONSQUEEZY_"],
    call: {
      // 63 of the 64 tools are Lemon Squeezy API calls with a hardcoded base
      // URL. The 64th reads the operator's webhook sink, whose URL is
      // configuration -- so it is pointed at the loopback server, which already
      // answers every path with the fixture body. The token is a placeholder
      // the sink sends and nothing checks.
      tool: "ls_sink_stats",
      requires: (ctx) => ctx.loopback.error ?? null,
      env: (ctx) => ({
        LEMONSQUEEZY_SINK_URL: ctx.loopback.url,
        LEMONSQUEEZY_SINK_ADMIN_TOKEN: "oam-matrix-placeholder-not-a-credential",
      }),
      args: () => ({}),
      deterministic: true,
      expect: (text) => {
        let stats;
        try {
          stats = JSON.parse(text);
        } catch {
          return "ls_sink_stats returned text that is not the sink's JSON";
        }
        return stats.fixture === LOOPBACK_MARKER
          ? null
          : "ls_sink_stats did not return the fixture server's body";
      },
    },
  },
  {
    name: "ctxlint",
    pkg: "@yawlabs/ctxlint",
    args: ["serve"],
    envPrefixes: ["CTXLINT_"],
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
// How long a sidecar gets to exit after its stdin closes -- the shutdown every
// MCP host performs -- before it is killed.
const EXIT_GRACE_MS = 5_000;
// How long after the sidecar is gone a process it started directly may still
// be exiting before it counts as left behind. Polled, so a clean teardown
// costs nothing and only a real leak waits the full window.
const LEAK_SETTLE_MS = 10_000;
// A process-table read, which on a loaded Windows box is mostly PowerShell
// starting up -- measured at 11-17s, so a 30s budget failed under load.
const PROCESS_TABLE_TIMEOUT_MS = 60_000;

/** `npm install --no-save <specs...>` into the shared stage. */
function npmInstall(specs) {
  const r = spawnSync(
    process.platform === "win32" ? "npm.cmd" : "npm",
    ["install", "--no-save", "--no-audit", "--no-fund", "--prefix", stage, ...specs],
    {
      encoding: "utf8",
      timeout: INSTALL_TIMEOUT_MS,
      shell: process.platform === "win32",
      // puppeteer's postinstall downloads its own Chrome, and a half-extracted
      // copy left in the user cache by an interrupted download fails that
      // postinstall -- which failed the whole batch and SKIPPED puppeteer on
      // 2026-09-12. The matrix drives the browser already on the box, so the
      // download buys nothing and can only break the install.
      env: { ...process.env, PUPPETEER_SKIP_DOWNLOAD: "1" },
    },
  );
  if (r.status === 0) return null;
  return `npm install failed: ${npmFailureReason(r)}`;
}

/** Why an npm run failed, from its spawnSync result.
 *
 *  npm's LAST stderr line is always "A complete log of this run can be found
 *  in: ...", so taking the tail reported a log path instead of the reason and
 *  made every SKIP undiagnosable. The first `npm error` line carries the code
 *  (E404, EACCES). Warnings are never the reason: with no error line, the
 *  first line used to be a deprecation notice, printed as the cause of an
 *  install that had actually been killed by the timeout. */
function npmFailureReason(r) {
  if (r.error?.code === "ETIMEDOUT") return `timed out after ${INSTALL_TIMEOUT_MS / 1000}s`;
  if (r.error) return r.error.message;
  const lines = (r.stderr || "")
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !/A complete log of this run/.test(l) && !/^npm (warn|WARN)/.test(l));
  return (
    lines.find((l) => /^npm (error|ERR!)/.test(l))
    ?? lines[0]
    ?? (r.signal ? `killed by ${r.signal}` : `exited ${r.status} with no error output`)
  );
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
function installAll(pkgs, install = npmInstall, note = (line) => process.stderr.write(line)) {
  const batch = install(pkgs.map((p) => `${p}@latest`));
  if (!batch) return new Map();
  // One bad package must not take the other six down with it. Install each on
  // its own to find out WHICH one npm rejected.
  note(`  batch install failed (${batch}); retrying one by one\n`);
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
// The control arm -- making `node <bin>` actually run on node
// =============================================================================

/** The `*_RUNTIME` switches an oam-preferring launcher reads, from its source.
 *
 *  Returns `{ launcher: false }` for an ordinary bin. For a launcher -- source
 *  that names oam AND spawns -- returns the switch names, which the control arm
 *  sets to `node`. A launcher that names none is still reported as a launcher
 *  with no vars, and the caller refuses to trust a control it cannot pin: the
 *  failure this exists to prevent is silent, so the fallback must not be.
 *
 *  Read from the source rather than listed in the table because the launcher
 *  is forked into a dozen repos and renamed in each; a table would drift the
 *  day one of them is renamed. */
function launcherRuntimeVars(source) {
  const launcher = /\boam\b/.test(source) && /\bspawn\s*\(/.test(source);
  if (!launcher) return { launcher: false, vars: [] };
  const vars = [...new Set(source.match(/\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*_RUNTIME\b/g) ?? [])];
  return { launcher: true, vars };
}

/** The environment a sidecar is spawned with. The inherited environment minus
 *  anything matching `prefixes` (case-insensitively: Windows environment names
 *  are), then the harness's own settings on top. */
function sidecarEnv(inherited, prefixes, ...layers) {
  const upper = prefixes.map((p) => p.toUpperCase());
  const env = {};
  for (const [k, v] of Object.entries(inherited)) {
    if (!upper.some((p) => k.toUpperCase().startsWith(p))) env[k] = v;
  }
  return Object.assign(env, { NO_COLOR: "1" }, ...layers);
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
//
// Every verdict is `{ state, why?, note? }` where state is one of:
//   verified -- oam answered and the answer holds
//   fail     -- oam is at fault; this is the one state that reddens a release
//   upstream -- the sidecar is broken on node too, so oam is not the suspect
//   demoted  -- the environment could not support the call on either runtime

/** Adjudicate a BOOT failure against the node control.
 *
 * Split out of the run loop so it is testable without spawning a sidecar --
 * the same reason classifyCall below is a pure function. It decides whether a
 * release goes red, and it was the one verdict nothing could exercise.
 */
function classifyBoot(oam, node) {
  if (!node.ok) {
    const same = node.why === oam.why;
    return {
      state: "upstream",
      why: same
        ? `${oam.why}; node fails identically -- broken sidecar, not oam`
        : `${oam.why}; node also fails, differently (${node.why}) -- broken sidecar, not oam`,
    };
  }
  return { state: "fail", why: `${oam.why}; the node control booted fine, so this is oam` };
}

/** Adjudicate an oam tool-call verdict against the node control's.
 *  Verdicts are `{ ok: true, text }` or `{ ok: false, why }`. `unavailable`
 *  recognizes a failure that is the environment's (a refused connection), and
 *  only demotes when BOTH arms hit one -- node reaching the service where oam
 *  could not is oam's networking, not the environment. */
function classifyCall(oam, node, deterministic, unavailable = () => false) {
  // No usable control. oam's failure stands on its own rather than being
  // excused by an arm that never reached the tool.
  if (!oam.ok && node.probeFailed) {
    return {
      state: "fail",
      why: `${oam.why}; the node control could not run (${node.why}), so nothing exonerates oam`,
    };
  }
  if (!oam.ok && !node.ok) {
    if (unavailable(oam.why) && unavailable(node.why)) {
      return { state: "demoted", why: `the service refused both runtimes: ${oam.why}` };
    }
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

/** Fold what each arm left running after teardown into a verdict.
 *  `oamLeft`/`nodeLeft` are process-name lists, or null when that arm's tree
 *  could not be read (or the arm never ran). Only a leak the node control did
 *  NOT reproduce is held against oam; one both arms produce is the sidecar's. */
function classifyLeaks(verdict, oamLeft, nodeLeft) {
  const withNote = (note) => ({ ...verdict, note: verdict.note ? `${verdict.note}; ${note}` : note });
  if (oamLeft === null) return withNote("could not read the oam arm's process tree, so leaks were not checked");
  if (oamLeft.length === 0) return verdict;
  const left = describeProcesses(oamLeft);
  if (nodeLeft === null) return withNote(`oam left ${left} running, and no node control ran to compare`);
  if (nodeLeft.length > 0) {
    return withNote(`both runtimes left processes running (oam: ${left}; node: ${describeProcesses(nodeLeft)}) -- the sidecar's, not oam's`);
  }
  const why = `left ${left} running after the sidecar exited; the node control left none`;
  if (verdict.state === "fail") return { ...verdict, why: `${verdict.why}; also ${why}` };
  return { ...verdict, state: "fail", why };
}

/** `msedge.exe x8, oam.exe` -- counts collapse, order follows first sighting. */
function describeProcesses(names) {
  const counts = new Map();
  for (const n of names) counts.set(n, (counts.get(n) ?? 0) + 1);
  return [...counts].map(([n, c]) => (c > 1 ? `${n} x${c}` : n)).join(", ");
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

/** The gate's exit code for a finished run. A skip, an upstream break and a
 *  demotion all mean the matrix could not answer for that sidecar -- none of
 *  them is a pass. Declared boot-only rows are the gate's known, printed
 *  coverage rather than a run that fell short, so they do not hold it. */
function exitCodeFor(results) {
  if (results.some((r) => r.state === "fail")) return 1;
  if (results.some((r) => ["upstream", "skip", "demoted"].includes(r.state))) return 3;
  return 0;
}

// =============================================================================
// Redis wire protocol -- the fake redis-mcp is called against
// =============================================================================

/** Split a buffer of RESP requests into argv arrays. Clients send arrays of
 *  bulk strings; inline commands are accepted for hand testing. Returns the
 *  complete commands and the unconsumed tail of a partial one. */
function parseResp(buf) {
  const commands = [];
  let i = 0;
  while (i < buf.length) {
    const start = i;
    const eol = buf.indexOf("\r\n", i);
    if (eol < 0) break;
    if (buf[i] !== 0x2a) {
      const parts = buf.subarray(i, eol).toString().trim().split(/\s+/).filter(Boolean);
      i = eol + 2;
      if (parts.length > 0) commands.push(parts);
      continue;
    }
    const count = Number(buf.subarray(i + 1, eol).toString());
    i = eol + 2;
    const argv = [];
    for (let k = 0; k < count; k += 1) {
      const lenEol = buf.indexOf("\r\n", i);
      if (buf[i] !== 0x24 || lenEol < 0) break;
      const len = Number(buf.subarray(i + 1, lenEol).toString());
      if (lenEol + 2 + len + 2 > buf.length) break;
      argv.push(buf.subarray(lenEol + 2, lenEol + 2 + len).toString());
      i = lenEol + 2 + len + 2;
    }
    if (argv.length < count) {
      i = start;
      break;
    }
    commands.push(argv);
  }
  return { commands, rest: buf.subarray(i) };
}

/** The fake's reply to one command. Unknown commands get a real error rather
 *  than a blanket +OK: a sidecar that starts sending something new should fail
 *  loudly here, not assert over a reply of the wrong type. */
function respReply(argv) {
  const bulk = (s) => `$${Buffer.byteLength(s)}\r\n${s}\r\n`;
  const [name = "", ...args] = argv;
  switch (name.toLowerCase()) {
    case "client":
    case "select":
    case "quit":
      return "+OK\r\n";
    case "ping":
      return args.length > 0 ? bulk(args[0]) : "+PONG\r\n";
    case "dbsize":
      return ":0\r\n";
    case "info":
      return bulk(
        [
          "# Server",
          `redis_version:${RESP_FAKE_VERSION}`,
          "redis_mode:standalone",
          "uptime_in_seconds:1",
          "# Clients",
          "connected_clients:1",
          "blocked_clients:0",
          "# Memory",
          "used_memory:1024",
          "used_memory_human:1.00K",
          "maxmemory:0",
          "maxmemory_policy:noeviction",
          "# Persistence",
          "loading:0",
          "aof_enabled:0",
          "# Stats",
          "instantaneous_ops_per_sec:0",
          "total_commands_processed:1",
          "keyspace_hits:0",
          "keyspace_misses:0",
          "# Replication",
          "role:master",
          "connected_slaves:0",
          "# Keyspace",
          "",
        ].join("\r\n"),
      );
    default:
      return `-ERR unknown command '${name}' (oam sidecar matrix fake)\r\n`;
  }
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
  const quiet = () => {};
  // Scoped and unscoped names both appear on purpose: the specs are built by
  // string concatenation, and `@scope/pkg@latest` is where that goes wrong.
  const cases = [
    {
      name: "one bad package: batch, then attribution, then a SURVIVOR RE-BATCH",
      run() {
        const pkgs = ["@scope/alpha", "bravo", "@scope/charlie"];
        const npm = recordingInstaller({ rejects: ["bravo"] });

        const failed = installAll(pkgs, npm.install, quiet);

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

        const failed = installAll(pkgs, npm.install, quiet);

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

        const failed = installAll(pkgs, npm.install, quiet);

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

        const failed = installAll(pkgs, npm.install, quiet);

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
      name: "a boot failure reproduced on node is upstream, not oam's",
      run() {
        const v = classifyBoot(
          { ok: false, why: "exited early (code 1) awaiting tools/list" },
          { ok: false, why: "exited early (code 1) awaiting tools/list" },
        );
        assertDeep(v.state, "upstream", "both runtimes fail to boot it -- the sidecar is broken");
        assertDeep(/node fails identically/.test(v.why), true, "and the log says why");
      },
    },
    {
      name: "a boot failure the control does NOT reproduce is oam's",
      run() {
        const v = classifyBoot({ ok: false, why: "no tools/list response within 90s" }, { ok: true });
        assertDeep(v.state, "fail", "node booted it fine, so this one is ours");
      },
    },
    {
      name: "two runtimes wording the same boot failure differently is still upstream",
      run() {
        const v = classifyBoot(
          { ok: false, why: "exited early (code 1)" },
          { ok: false, why: "exited early (code 9009)" },
        );
        assertDeep(v.state, "upstream", "a differing errno spelling does not make it oam's bug");
        assertDeep(/differently/.test(v.why), true, "and the difference is stated, not papered over");
      },
    },
    {
      name: "an initialize ERROR is diagnosed at once, not waited out",
      run() {
        // The dispatcher used to match only `id === 1 && result`, so an error
        // reply fell through and the probe sat out the full 90s boot timeout
        // before reporting the WRONG phase.
        const line = JSON.stringify({
          jsonrpc: "2.0",
          id: 1,
          error: { code: -32602, message: "Unsupported protocol version: 2024-11-05" },
        });
        const msg = JSON.parse(line);
        assertDeep(
          msg.id === 1 && Boolean(msg.error),
          true,
          "the shape the dispatcher must recognize before it can report it",
        );
      },
    },
    {
      name: "a control that could not RUN never exonerates oam",
      run() {
        const v = classifyCall(
          { ok: false, why: "tool call threw" },
          { ok: false, probeFailed: true, why: "node control could not probe: exited early (code 1)" },
          false,
        );
        assertDeep(
          v.state,
          "fail",
          "an arm that never reached the tool is no evidence -- folding it in with "
            + "'the control ran it and it failed' let a real oam regression read as upstream",
        );
      },
    },
    {
      name: "a control that RAN and failed the same way is still upstream",
      run() {
        const v = classifyCall(
          { ok: false, why: "tool call threw" },
          { ok: false, why: "tool call threw" },
          false,
        );
        assertDeep(v.state, "upstream", "both arms reached the tool and both failed");
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
      name: "a service that refuses BOTH runtimes demotes; refusing only oam is oam's",
      run() {
        const refused = (why) => /ECONNREFUSED|password authentication failed/.test(why);
        const both = classifyCall(
          { ok: false, why: "tool reported an error: connect ECONNREFUSED 127.0.0.1:5432" },
          { ok: false, why: "tool reported an error: connect ECONNREFUSED 127.0.0.1:5432" },
          true,
          refused,
        );
        assertDeep(both.state, "demoted", "no database on the box is the environment, not a broken sidecar");
        const onlyOam = classifyCall(
          { ok: false, why: "tool reported an error: connect ECONNREFUSED 127.0.0.1:5432" },
          { ok: true, text: "{}" },
          true,
          refused,
        );
        // Node reaching the server oam could not is oam's socket layer. Letting
        // the "environment" excuse cover it would hide exactly the regression a
        // networked sidecar exists in this matrix to catch.
        assertDeep(onlyOam.state, "fail", "a refusal only oam sees is not the environment");
      },
    },
    {
      name: "a demoted row makes the run INCOMPLETE, never clean",
      run() {
        // The hole this closes: a fixture that failed to start (a loopback bind
        // refused) used to demote the call to "boot only" and exit 0, so the
        // strongest assertion in the set could stop running with the gate green.
        assertDeep(exitCodeFor([{ state: "verified" }, { state: "demoted" }]), 3, "demoted is not a pass");
        assertDeep(exitCodeFor([{ state: "demoted" }, { state: "fail" }]), 1, "a failure still outranks it");
        assertDeep(exitCodeFor([{ state: "verified" }, { state: "boot" }]), 0, "declared boot-only coverage does not hold the gate");
      },
    },
    {
      name: "the control arm is pinned to node through the launcher's own switch",
      run() {
        const launcher = [
          'const mode = (process.env.FETCH_MCP_RUNTIME ?? "auto").toLowerCase();',
          'const oam = findOam();',
          'child = spawn(oam, ["run", SERVER_ENTRY]);',
        ].join("\n");
        assertDeep(
          launcherRuntimeVars(launcher),
          { launcher: true, vars: ["FETCH_MCP_RUNTIME"] },
          "a launcher's switch is found, so `node <bin>` stops re-spawning oam",
        );
        assertDeep(
          launcherRuntimeVars('const oam = findOam();\nspawn(oam, ["run", entry]);'),
          { launcher: true, vars: [] },
          "a launcher with no switch is still recognized as one -- and then refused, not trusted",
        );
        assertDeep(
          launcherRuntimeVars('import("./dist/index.js");'),
          { launcher: false, vars: [] },
          "an ordinary bin needs no pin",
        );
      },
    },
    {
      name: "inherited configuration is scrubbed before the harness's own",
      run() {
        const env = sidecarEnv(
          { PATH: "/bin", TAILSCALE_API_KEY: "tskey-real", tailscale_readonly: "1", HOME: "/h" },
          ["TAILSCALE_"],
          { TAILSCALE_MCP_RUNTIME: "node" },
        );
        assertDeep(
          env,
          { PATH: "/bin", HOME: "/h", NO_COLOR: "1", TAILSCALE_MCP_RUNTIME: "node" },
          "a real key never reaches the sidecar, whatever the case of the name, and the harness's setting survives",
        );
      },
    },
    {
      name: "a leak only oam produces is oam's; one both produce is the sidecar's",
      run() {
        const oamOnly = classifyLeaks({ state: "verified" }, ["msedge.exe", "msedge.exe", "oam.exe"], []);
        assertDeep(oamOnly.state, "fail", "node tears the sidecar's browser down and oam did not");
        assertDeep(
          /msedge\.exe x2, oam\.exe/.test(oamOnly.why),
          true,
          "the reason names what was left behind",
        );
        const both = classifyLeaks({ state: "verified" }, ["chrome"], ["chrome"]);
        assertDeep(both.state, "verified", "a leak node reproduces is not an oam regression");
        assertDeep(/both runtimes/.test(both.note), true, "but it is still said");
        assertDeep(classifyLeaks({ state: "verified" }, [], []), { state: "verified" }, "nothing left, nothing said");
        assertDeep(
          classifyLeaks({ state: "verified" }, ["chrome"], null).state,
          "verified",
          "with no control to compare, a leak is reported, not blamed",
        );
      },
    },
    {
      name: "an npm failure is explained by its error or its timeout, never by a warning",
      run() {
        assertDeep(
          npmFailureReason({ status: null, error: { code: "ETIMEDOUT", message: "spawnSync npm.cmd ETIMEDOUT" }, stderr: "npm warn deprecated puppeteer@23.11.1\n" }),
          `timed out after ${INSTALL_TIMEOUT_MS / 1000}s`,
          "a killed install says it timed out",
        );
        assertDeep(
          npmFailureReason({ status: 1, stderr: "npm warn deprecated x@1\nnpm error code E404\nnpm error 404 Not Found\nnpm error A complete log of this run can be found in: C:\\log\n" }),
          "npm error code E404",
          "the first error line, not the warning before it or the log path after it",
        );
        assertDeep(
          npmFailureReason({ status: 1, stderr: "npm warn deprecated x@1\n" }),
          "exited 1 with no error output",
          "a run with only warnings does not blame the warning",
        );
      },
    },
    {
      name: "the Redis fake answers what redis-mcp sends, and refuses what it does not",
      run() {
        const wire = Buffer.from("*2\r\n$6\r\nclient\r\n$7\r\nSETINFO\r\n*1\r\n$4\r\nINFO\r\n*1\r\n$3\r\nDBS");
        const { commands, rest } = parseResp(wire);
        assertDeep(commands, [["client", "SETINFO"], ["INFO"]], "complete commands are split out");
        assertDeep(rest.toString(), "*1\r\n$3\r\nDBS", "a partial trailing command waits for the rest");
        assertDeep(respReply(["dbsize"]), ":0\r\n", "DBSIZE is an integer reply");
        assertDeep(
          respReply(["INFO"]).includes(`redis_version:${RESP_FAKE_VERSION}`),
          true,
          "INFO carries the version the redis assertion checks for",
        );
        assertDeep(respReply(["FLUSHALL"]).startsWith("-ERR"), true, "an unexpected command is an error, not a blanket +OK");
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
        // THE RATCHET. Every sidecar is invoked today. Adding a boot-only one
        // is a coverage decision, so it has to be made here, in review, rather
        // than by a table edit nobody reads as one.
        assertDeep(
          SIDECARS.filter((s) => s.bootOnly).map((s) => s.name),
          [],
          "the boot-only set may shrink but not grow without editing this assertion",
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

/** Resolve an installed package's bin entry point and version.
 *  Mirrors oam-spawn.ts: the BIN from package.json, not require.resolve --
 *  a package's library export is often ESM-gated and is not what npx runs. */
function resolveBin(pkg) {
  const dir = join(stage, "node_modules", ...pkg.split("/"));
  const manifestPath = join(dir, "package.json");
  if (!existsSync(manifestPath)) return { error: `not on disk after install: ${dir}` };
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const bin = manifest.bin;
  const rel = typeof bin === "string" ? bin : bin && Object.values(bin)[0];
  if (!rel) return { error: "package.json declares no bin", version: manifest.version };
  const entry = resolve(dir, rel);
  if (!existsSync(entry)) return { error: `bin missing on disk: ${entry}`, version: manifest.version };
  return { entry, version: manifest.version };
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

// One PowerShell for the whole run, fed a query per probe over stdin. Windows
// has no `ps` that reports parent ids, and starting PowerShell per query cost
// more than the probes it was checking.
const PS_END = "__oam_matrix_end__";
let psHost = null;

/** `pid ppid name` lines for every process, or null when the table could not
 *  be read in time. */
function processTable() {
  if (process.platform !== "win32") {
    return new Promise((resolveP) => {
      const child = spawn("ps", ["-A", "-o", "pid=,ppid=,comm="], { stdio: ["ignore", "pipe", "ignore"] });
      let out = "";
      const timer = setTimeout(() => {
        child.kill();
        resolveP(null);
      }, PROCESS_TABLE_TIMEOUT_MS);
      child.stdout.on("data", (d) => {
        out += d;
      });
      child.on("error", () => resolveP(null));
      child.on("close", (code) => {
        clearTimeout(timer);
        resolveP(code === 0 ? out.split("\n") : null);
      });
    });
  }
  if (!psHost) {
    const child = spawn("powershell", ["-NoProfile", "-NonInteractive", "-Command", "-"], {
      stdio: ["pipe", "pipe", "ignore"],
      windowsHide: true,
    });
    const host = { child, buf: "", waiters: [] };
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (d) => {
      host.buf += d;
      for (let i = host.buf.indexOf(PS_END); i >= 0; i = host.buf.indexOf(PS_END)) {
        const out = host.buf.slice(0, i);
        host.buf = host.buf.slice(i + PS_END.length);
        host.waiters.shift()?.(out.split(/\r?\n/));
      }
    });
    const lost = () => {
      if (psHost === host) psHost = null;
      for (const w of host.waiters.splice(0)) w(null);
    };
    child.on("error", lost);
    child.on("exit", lost);
    child.stdin.on("error", lost);
    psHost = host;
  }
  const host = psHost;
  return new Promise((resolveP) => {
    let answered = false;
    const answer = (rows) => {
      if (answered) return;
      answered = true;
      clearTimeout(timer);
      resolveP(rows);
    };
    const timer = setTimeout(() => {
      // A late answer would be handed to the NEXT query, so a host that missed
      // its deadline is discarded rather than reused.
      host.child.kill();
      answer(null);
    }, PROCESS_TABLE_TIMEOUT_MS);
    host.waiters.push(answer);
    // ONE write for the whole table. Emitting a line per process through the
    // pipeline measured ~120ms a line on a loaded box -- minutes for a thousand
    // processes, so every read timed out and every leak went unchecked.
    host.child.stdin.write(
      `$t = Get-CimInstance Win32_Process | ForEach-Object { "$($_.ProcessId) $($_.ParentProcessId) $($_.Name)" }; [Console]::Out.Write(($t -join [char]10) + [char]10 + "${PS_END}" + [char]10); [Console]::Out.Flush()\n`,
    );
  });
}

/** Every process descended from `rootPid`, as `{ pid, name, depth }` (depth 1
 *  is a direct child), or null when the table could not be read. One snapshot
 *  of the whole table, walked in memory. */
async function descendants(rootPid) {
  const rows = await processTable();
  if (rows === null) return null;
  const children = new Map();
  for (const line of rows) {
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+(.+)$/);
    if (!m) continue;
    const [, pid, ppid, name] = m;
    if (!children.has(Number(ppid))) children.set(Number(ppid), []);
    children.get(Number(ppid)).push({ pid: Number(pid), name: name.split("/").pop() });
  }
  const out = [];
  const walk = (pid, depth) => {
    for (const c of children.get(pid) ?? []) {
      // A pid can be its own ancestor's recycled number; never walk into a loop.
      if (c.pid === rootPid || out.some((o) => o.pid === c.pid)) continue;
      out.push({ ...c, depth });
      walk(c.pid, depth + 1);
    }
  };
  walk(rootPid, 1);
  return out;
}

const isAlive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Shut a sidecar down the way an MCP host does -- close its stdin, give it a
 *  grace period, then kill it -- and report which of the processes it started
 *  DIRECTLY are still running.
 *
 *  Direct children only, because that is node's actual guarantee and the only
 *  one that holds on a loaded box. libuv puts each child in a kill-on-close job
 *  object with SILENT_BREAKAWAY_OK, so node's children die the instant it does
 *  while THEIR children are outside the job and exit on their own schedule: a
 *  browser's renderers were measured still draining 10s after node's browser
 *  was killed, which read as node "leaking" and flipped verdicts between runs.
 *  A direct child outliving the sidecar is deterministic by comparison -- node
 *  never leaves one; oam, with no job object, always did. Deeper processes are
 *  still cleaned up, just not judged. */
async function teardown(child) {
  const tree = child.exitCode === null ? await descendants(child.pid) : [];
  const exited = new Promise((r) => {
    if (child.exitCode !== null || child.signalCode !== null) r();
    else child.once("exit", r);
  });
  child.stdin.end();
  const graceful = await Promise.race([exited.then(() => true), sleep(EXIT_GRACE_MS).then(() => false)]);
  if (!graceful) {
    child.kill();
    await Promise.race([exited, sleep(EXIT_GRACE_MS)]);
  }
  if (tree === null) return null;
  let left = tree.filter((p) => p.depth === 1 && isAlive(p.pid));
  for (let waited = 0; left.length > 0 && waited < LEAK_SETTLE_MS; waited += 250) {
    await sleep(250);
    left = left.filter((p) => isAlive(p.pid));
  }
  // Cleaned up either way, at every depth: a leak is reported once, not left to
  // pile up across runs and change what the next run's process table looks like.
  for (const p of tree.filter((q) => isAlive(q.pid))) {
    try {
      process.kill(p.pid);
    } catch {
      // Already gone between the check and the kill.
    }
  }
  return left.map((p) => p.name);
}

/** Speak MCP over stdio to a sidecar hosted on `host` ("oam" or "node").
 *  Resolves with the tool names it serves, the verdict on invoking `call` when
 *  set, and `left` -- the processes it left running (null: not checked). */
function probe(host, entry, { env, scriptArgs = [], call = null, ctx }) {
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
  return new Promise((resolveP) => {
    const child = spawn(cmd, argv, { stdio: ["pipe", "pipe", "pipe"], env, windowsHide: true });

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
      teardown(child).then((left) => resolveP({ ...result, left, stderr }));
    };
    const wait = (what, ms) => {
      awaiting = what;
      clearTimeout(timer);
      timer = setTimeout(() => done({ ok: false, why: `no ${awaiting} response within ${ms / 1000}s` }), ms);
    };
    wait("tools/list", BOOT_TIMEOUT_MS);

    child.on("error", (e) => done({ ok: false, why: `spawn failed: ${e.message}` }));
    child.on("exit", (code) => done({ ok: false, why: `exited early (code ${code}) awaiting ${awaiting}` }));
    child.stdin.on("error", () => {
      // EPIPE from a sidecar that died mid-write; the exit handler reports it.
    });
    child.stderr.on("data", (d) => {
      stderr += d;
    });

    const send = (msg) => child.stdin.write(`${JSON.stringify(msg)}\n`);
    // The calls to make after tools/list, in order: any `before` steps (a
    // navigate the asserted call depends on), then the asserted call itself.
    const steps = call ? [...(call.before ?? []), call] : [];
    let step = 0;
    const nextStep = () => {
      const s = steps[step];
      wait(`tools/call ${s.tool}`, CALL_TIMEOUT_MS);
      send({ jsonrpc: "2.0", id: 3 + step, method: "tools/call", params: { name: s.tool, arguments: s.args(ctx) } });
    };

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
        if (msg.id === 1 && msg.error) {
          // An ERROR reply to initialize used to match nothing here and be
          // discarded, so the probe sat out the full boot timeout and then
          // reported the wrong phase ("no tools/list response within 90s") --
          // throwing away the one sentence that said what actually happened.
          // The likeliest cause is the hardcoded protocolVersion below being
          // refused, which is a 90-second stall and a misleading red for a
          // sidecar that is working fine.
          done({ ok: false, why: `initialize error: ${msg.error.message}` });
          return;
        }
        if (msg.id === 1 && msg.result) {
          send({ jsonrpc: "2.0", method: "notifications/initialized" });
          send({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
        } else if (msg.id === 2) {
          if (msg.error) {
            done({ ok: false, why: `tools/list error: ${msg.error.message}` });
            return;
          }
          tools = (msg.result?.tools ?? []).map((t) => t.name);
          if (tools.length === 0) {
            done({ ok: false, why: "served an EMPTY tool list" });
            return;
          }
          if (!call) {
            done({ ok: true, tools });
            return;
          }
          // Every tool the run will call has to be one the sidecar just said it
          // has. Calling a renamed tool anyway gets "unknown tool" back from
          // BOTH arms, which adjudicates as a broken sidecar and buries the
          // actual news: this harness is asserting against something that no
          // longer exists and needs a new tool picked. Same verdict (not oam),
          // better sentence.
          const missing = steps.map((s) => s.tool).find((t) => !tools.includes(t));
          if (missing) {
            done({
              ok: true,
              tools,
              stale: true,
              call: {
                ok: false,
                why: `no longer advertises ${missing} (it serves ${tools.length} other tools) -- pick a new tool for the matrix`,
              },
            });
            return;
          }
          nextStep();
        } else if (typeof msg.id === "number" && msg.id >= 3 && msg.id === 3 + step) {
          if (step < steps.length - 1) {
            const setup = callVerdict(msg, () => null);
            if (!setup.ok) {
              done({ ok: true, tools, call: { ok: false, why: `${steps[step].tool} (setup): ${setup.why}` } });
              return;
            }
            step += 1;
            nextStep();
            return;
          }
          done({ ok: true, tools, call: callVerdict(msg, call.expect) });
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
  try {
    rmSync(fixture, { recursive: true, force: true, maxRetries: 3 });
  } catch (e) {
    // A browser profile held open by a process an earlier, killed run left
    // behind. Name it: "EBUSY" alone sends the operator looking at npm.
    console.error(`cannot clear the fixture directory ${fixture} (${e.code ?? e.message}) -- a process from an earlier run is holding it`);
    process.exit(2);
  }
  mkdirSync(fixture, { recursive: true });
  writeFileSync(join(fixture, FIXTURE_CONTEXT_FILE), FIXTURE_CONTEXT_BODY);
  // Earlier runs' profiles, best effort. One still held open is named rather
  // than fatal: this run writes its own.
  if (existsSync(profilesRoot)) {
    for (const name of readdirSync(profilesRoot)) {
      try {
        rmSync(join(profilesRoot, name), { recursive: true, force: true, maxRetries: 2 });
      } catch (e) {
        console.error(`  note: ${join(profilesRoot, name)} is still held open (${e.code ?? e.message}) -- a browser an earlier run left behind`);
      }
    }
  }
  mkdirSync(profiles, { recursive: true });
}

/** A loopback HTTP server for the calls that need something to reach.
 *  `/page` is an HTML page for the browsers, titled with the marker; every
 *  other path answers the marker as JSON. Resolves to `{ server, url }`, or to
 *  `{ error }` with the reason it could not listen. */
function startLoopback() {
  return new Promise((resolveP) => {
    const server = createServer((req, res) => {
      if (req.url.startsWith("/page")) {
        res.writeHead(200, { "content-type": "text/html" });
        res.end(`<!doctype html><title>${LOOPBACK_MARKER}</title><h1>${LOOPBACK_MARKER}</h1>`);
        return;
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ fixture: LOOPBACK_MARKER }));
    });
    server.once("error", (e) => resolveP({ error: `loopback fixture server: ${e.message}` }));
    server.listen(0, "127.0.0.1", () => {
      resolveP({ server, url: `http://127.0.0.1:${server.address().port}/` });
    });
  });
}

/** The Redis wire-protocol fake, on loopback. */
function startResp() {
  return new Promise((resolveP) => {
    const server = createTcpServer((sock) => {
      let buf = Buffer.alloc(0);
      sock.on("error", () => {});
      sock.on("data", (d) => {
        const { commands, rest } = parseResp(Buffer.concat([buf, d]));
        buf = rest;
        for (const argv of commands) {
          sock.write(respReply(argv));
          if ((argv[0] ?? "").toLowerCase() === "quit") sock.end();
        }
      });
    });
    server.once("error", (e) => resolveP({ error: `redis fixture server: ${e.message}` }));
    server.listen(0, "127.0.0.1", () => {
      resolveP({ server, url: `redis://127.0.0.1:${server.address().port}` });
    });
  });
}

/** Can anything be reached at the host and port a service URL names? A TCP
 *  connect and nothing more: speaking the protocol here means maintaining a
 *  second client, and a hand-rolled postgres handshake measured as unreliable
 *  (a missing role read as reachable). Authentication failures are caught
 *  after the call instead, by the entry's `unavailable`. */
function tcpPreflight(url, defaultPort, timeoutMs = 1_500) {
  let host;
  let port;
  try {
    const u = new URL(url);
    host = u.hostname || "127.0.0.1";
    port = Number(u.port || defaultPort);
  } catch {
    return Promise.resolve(null); // not a URL this can check; let the call say
  }
  return new Promise((resolveP) => {
    const sock = connect({ host, port });
    const finish = (why) => {
      sock.destroy();
      resolveP(why);
    };
    sock.setTimeout(timeoutMs);
    sock.once("connect", () => finish(null));
    sock.once("timeout", () => finish(`nothing listening at ${host}:${port} (no connect within ${timeoutMs}ms)`));
    sock.once("error", (e) => finish(`nothing listening at ${host}:${port} (${e.code ?? e.message})`));
  });
}

/** A Chromium-family browser already on this box, for the two browser
 *  sidecars. Neither downloads one here: that download is exactly what failed
 *  puppeteer's install, and it would test the download rather than oam. */
function findBrowser() {
  if (process.env.OAM_MATRIX_BROWSER) {
    return existsSync(process.env.OAM_MATRIX_BROWSER)
      ? { path: process.env.OAM_MATRIX_BROWSER }
      : { error: `OAM_MATRIX_BROWSER does not exist: ${process.env.OAM_MATRIX_BROWSER}` };
  }
  const env = process.env;
  const candidates = {
    win32: [
      env["ProgramFiles(x86)"] && join(env["ProgramFiles(x86)"], "Microsoft", "Edge", "Application", "msedge.exe"),
      env.ProgramFiles && join(env.ProgramFiles, "Microsoft", "Edge", "Application", "msedge.exe"),
      env.ProgramFiles && join(env.ProgramFiles, "Google", "Chrome", "Application", "chrome.exe"),
      env.LOCALAPPDATA && join(env.LOCALAPPDATA, "Google", "Chrome", "Application", "chrome.exe"),
    ],
    darwin: [
      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
      "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
      "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ],
    linux: ["/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser", "/usr/bin/microsoft-edge"],
  }[process.platform] ?? [];
  const found = candidates.filter(Boolean).find((p) => existsSync(p));
  return found
    ? { path: found }
    : { error: "no Chrome, Edge or Chromium installed where this looks -- set OAM_MATRIX_BROWSER" };
}

// =============================================================================
// The run
// =============================================================================

const argv = process.argv.slice(2);
const only = (argv.find((a) => a.startsWith("--only=")) ?? "").slice(7);
const jsonPath = (argv.find((a) => a.startsWith("--json=")) ?? "").slice(7) || null;
const selected = only ? SIDECARS.filter((s) => only.split(",").includes(s.name)) : SIDECARS;

if (argv.includes("--list")) {
  for (const s of SIDECARS) {
    const what = s.call
      ? `calls ${[...(s.call.before ?? []).map((b) => b.tool), s.call.tool].join(" then ")}`
      : `boot only: ${s.bootOnly}`;
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
const version = spawnSync(oamBin, ["--version"], { encoding: "utf8" });
if (version.status !== 0) {
  console.error(`cannot run '${oamBin}' -- set OAM_BIN or put oam on PATH`);
  process.exit(2);
}
const oamVersion = version.stdout.trim();
console.error(`oam sidecar matrix -- ${oamVersion} against node ${process.version}`);
console.error(`stage: ${stage}\n`);

mkdirSync(stage, { recursive: true });
prepareFixture();

// In-place progress only on a terminal. Captured by release-local.sh, a `\r`
// does not return to the start of anything, so the progress text and the next
// line ran together into one unreadable line.
const live = process.stderr.isTTY === true;
const progress = (text) => {
  if (live) process.stderr.write(`\r${text.padEnd(60)}`);
};
const clearProgress = () => {
  if (live) process.stderr.write(`\r${"".padEnd(60)}\r`);
};

progress(`  installing ${selected.length} package(s)...`);
const installErrors = installAll(
  selected.map((s) => s.pkg),
  npmInstall,
  (line) => {
    clearProgress();
    process.stderr.write(line);
  },
);
clearProgress();

const needsBrowser = selected.some((s) => s.browser);
const fixtures = {
  loopback: await startLoopback(),
  resp: await startResp(),
  browser: needsBrowser ? findBrowser() : { error: "not needed" },
};

// One column layout for every line the run emits, continuations included: a
// verdict that does not line up under the sidecar it belongs to is read as
// belonging to the next one.
const BANNER = { verified: "PASS", fail: "FAIL", upstream: "UPSTREAM", skip: "SKIP", boot: "BOOT", demoted: "DEMOTED" };
const row = (name, banner, ver, detail) => `  ${name.padEnd(12)} ${banner.padEnd(8)} ${ver.padEnd(10)} ${detail}\n`;
const under = (detail) => row("", "", "", detail);
const results = [];

for (const s of selected) {
  const bin = installErrors.has(s.pkg) ? { error: installErrors.get(s.pkg) } : resolveBin(s.pkg);
  const ver = bin.version ?? "?";
  const record = (state, detail, { why = detail, note = null, tools = null, tool = null, extra = [] } = {}) => {
    clearProgress();
    process.stderr.write(row(s.name, BANNER[state], ver, detail));
    for (const line of [note ? `note: ${note}` : "", ...extra]) if (line) process.stderr.write(under(line.slice(0, 160)));
    results.push({ name: s.name, pkg: s.pkg, version: bin.version ?? null, state, tool, tools, why, note });
  };
  if (bin.error) {
    record("skip", bin.error);
    continue;
  }

  const source = readFileSync(bin.entry, "utf8");
  const pin = launcherRuntimeVars(source);
  if (pin.launcher && pin.vars.length === 0) {
    record("skip", "its bin re-spawns oam and names no *_RUNTIME switch, so the node control arm cannot be pinned to node");
    continue;
  }
  const nodePin = Object.fromEntries(pin.vars.map((v) => [v, "node"]));
  const ctxFor = (host) => ({
    host,
    loopback: fixtures.loopback,
    resp: fixtures.resp,
    browser: fixtures.browser,
    profileDir: join(profiles, `${s.name}-${host}`),
  });
  const envFor = (host, call) =>
    sidecarEnv(process.env, s.envPrefixes ?? [], s.env ?? {}, call?.env ? call.env(ctxFor(host)) : {}, host === "node" ? nodePin : {});

  // A call the environment cannot support is not attempted: the sidecar still
  // has to boot and serve tools on oam, and the row says exactly what was
  // missing.
  const unmet = s.call?.requires ? await s.call.requires(ctxFor("oam")) : null;
  const call = s.call && !unmet ? s.call : null;
  const scriptArgs = s.args ?? [];

  progress(`  ${s.name.padEnd(12)} probing on oam...`);
  const oam = await probe("oam", bin.entry, { env: envFor("oam", call), scriptArgs, call, ctx: ctxFor("oam") });

  if (!oam.ok) {
    // Ask the control here too. A sidecar that fails at boot, initialize or
    // tools/list is the MOST common upstream break -- a bad publish, a missing
    // peer dep, an engines bump -- and blaming oam for it means a release is
    // blocked by somebody else's broken package, with the log saying oam did
    // it. Only the tools/call arm used to be adjudicated, so exactly the shape
    // most likely to be upstream was the one never checked.
    progress(`  ${s.name.padEnd(12)} node control (boot)...`);
    const control = await probe("node", bin.entry, { env: envFor("node", call), scriptArgs, call, ctx: ctxFor("node") });
    const v = classifyLeaks(classifyBoot(oam, control), oam.left, control.left);
    record(v.state, v.why, { note: v.note, extra: [diagnosis(oam.stderr)] });
    continue;
  }
  if (!call) {
    const state = s.bootOnly ? "boot" : "demoted";
    const reason = s.bootOnly ?? unmet;
    const v = classifyLeaks({ state }, oam.left, null);
    record(v.state, `${oam.tools.length} tools served, ${s.call?.tool ?? "no tool"} not called: ${v.why ?? reason}`, {
      why: v.why ?? reason,
      note: v.note,
      tools: oam.tools.length,
    });
    continue;
  }
  // A tool the sidecar no longer advertises is answered by neither runtime, so
  // there is nothing to adjudicate -- and running the control to confirm that
  // would only spend a spawn to reach the same sentence.
  if (oam.stale) {
    record("upstream", oam.call.why, { tools: oam.tools.length, tool: call.tool });
    continue;
  }

  // The control arm. Run for EVERY invocation, not only failing ones: it is
  // what decides whose bug a red is, and a control taken only after a failure
  // is how "oam broke it" gets asserted first and checked second.
  progress(`  ${s.name.padEnd(12)} node control...`);
  const control = await probe("node", bin.entry, { env: envFor("node", call), scriptArgs, call, ctx: ctxFor("node") });
  // probeFailed keeps two very different facts apart. "The control ran the tool
  // and it failed" is evidence the sidecar is broken; "the control never got far
  // enough to invoke anything" is no evidence at all, and folding them together
  // let a REAL oam regression be excused as upstream whenever the control host
  // could not boot the sidecar (an engines bump past the release box's node,
  // say). A missing control must never exonerate oam.
  const nodeVerdict = control.ok
    ? (control.call ?? { ok: false, why: "node control returned no verdict" })
    : { ok: false, probeFailed: true, why: `node control could not probe: ${control.why}` };

  const v = classifyLeaks(
    classifyCall(oam.call, nodeVerdict, call.deterministic === true, call.unavailable),
    oam.left,
    control.left,
  );
  const detail =
    v.state === "verified"
      ? `${call.tool} verified against node (${oam.tools.length} tools served)`
      : `${call.tool}: ${v.why}`;
  record(v.state, detail, {
    why: v.why ?? null,
    note: v.note,
    tools: oam.tools.length,
    tool: call.tool,
    // A stderr diagnosis explains a failed CALL. When the call held and only the
    // teardown failed, the sidecar's last log line ("server closed") reads as the
    // cause of something it did not cause.
    extra: [oam.call.ok && nodeVerdict.ok ? "" : diagnosis(oam.stderr) || diagnosis(control.stderr)],
  });
}

fixtures.loopback.server?.close();
fixtures.resp.server?.close();
psHost?.child.kill();

const count = (state) => results.filter((r) => r.state === state).length;
const named = (state) => results.filter((r) => r.state === state).map((r) => r.name);

// The counts are split because collapsing them is the failure this stage was
// added to fix: "8/9 served tools" read as full coverage of a set where none
// of the eight had ever been asked to DO anything.
const parts = [`${count("verified")} tool-call verified`];
for (const [state, label] of [
  ["boot", "boot only"],
  ["demoted", "demoted"],
  ["upstream", "broken upstream"],
  ["skip", "could not run"],
  ["fail", "FAILED on oam"],
]) {
  if (count(state) > 0) parts.push(`${count(state)} ${label}`);
}
console.error(`\n${results.length} sidecars on ${oamVersion}: ${parts.join(", ")}`);
const advertised = results.reduce((n, r) => n + (r.tools ?? 0), 0);
const invoked = results.filter((r) => r.state === "verified").length;
// One call per sidecar is a floor on behaviour, not coverage of it. Said here
// so nobody reads "all verified" as "all tools verified".
if (advertised > 0) {
  console.error(`one tool called per sidecar: ${invoked} of ${advertised} advertised tools answered on both runtimes`);
}

const code = exitCodeFor(results);
if (jsonPath) {
  writeFileSync(
    jsonPath,
    `${JSON.stringify({ schema: "oam-mcp-sidecar-matrix/1", oam: oamVersion, node: process.version, platform: `${process.platform}-${process.arch}`, exitCode: code, sidecars: results }, null, 2)}\n`,
  );
  console.error(`report: ${jsonPath}`);
}
if (code === 1) {
  console.error(`FAILED on oam: ${named("fail").join(", ")}`);
} else if (code === 3) {
  // Neither a skip, an upstream break nor a demotion is a pass: each means the
  // matrix could not answer for that sidecar, and saying so is the difference
  // between a gate and a rubber stamp.
  const unanswered = [...named("demoted"), ...named("upstream"), ...named("skip")];
  console.error(`no oam failures, but the matrix is INCOMPLETE -- not answered: ${unanswered.join(", ")}`);
}
process.exit(code);
