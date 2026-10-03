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
// fails. The broker itself has a row too, launched the way `yaw-mcp install`
// writes it into a client config: `oam run --no-check <dist/index.js>`. Then it speaks MCP over stdio: initialize, notifications/initialized,
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
//                                                       # no network, no npm, no oam;
//                                                       # ci-local.sh step 13 runs it
//   OAM_BIN=/path/to/oam node scripts/mcp-sidecar-matrix.mjs
//   OAM_MATRIX_BROWSER=/path/to/chrome     # browser for puppeteer + playwright
//   OAM_MATRIX_DATABASE_URL=postgresql://  # default postgres@127.0.0.1:5432
//
// Exit code is the gate: 0 when every selected sidecar answered its tool call
// on oam; 1 when oam failed one; 3 when the matrix could not answer for one
// (install failure, an install still damaged after a rebuild, broken
// upstream, demoted fixture); 2 for bad usage, or when another run has held
// the shared stage for longer than this one will wait. An interrupt (Ctrl-C,
// Ctrl-Break, SIGTERM, SIGHUP) ends npm first; then the matrix dies of the
// signal (on Windows, exits STATUS_CONTROL_C_EXIT), so a calling script sees
// an interrupt -- which release-local.sh treats as fatal. The --json report is
// written last, only once there is a verdict.
// =============================================================================

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { rm } from "node:fs/promises";
import { createServer } from "node:http";
import { connect, createServer as createTcpServer } from "node:net";
import { basename, dirname, join, resolve } from "node:path";
import { constants as osConstants, tmpdir } from "node:os";

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
// What npm is handed as its script shell and as its git: two paths that are
// never created. The install runs with --ignore-scripts, and anything that
// would still run package code -- a git dependency's prepare, which npm 10
// runs even then (measured) -- fails at spawn with ENOENT instead, and npm's
// `syscall spawn <path>` line names which of the two it was (refusedReason).
const NO_SCRIPT_SHELL = join(stage, "lifecycle-scripts-are-not-run-by-the-matrix");
const NO_GIT = join(stage, "git-is-not-run-by-the-matrix");
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
// Plus the broker that does the rewriting (`yaw-mcp`), which is in no bundle:
// it is the process every client config launches.
//
// `oamFlags` go between `run` and the entry, on the oam arm only: the node arm
// is `node <entry>` whatever the row says. `isolateHome` points HOME,
// USERPROFILE, APPDATA and LOCALAPPDATA at a scratch home per arm, and runs
// the arm from inside it, for a sidecar that would otherwise read the box's
// real config.
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
      // Reaching loopback takes two switches, and the sidecar refuses the call
      // before any dial unless BOTH are set: the per-call `allow_private_hosts`
      // argument, and FETCH_MCP_ALLOW_PRIVATE_HOSTS=1 in the server's
      // environment. fetch-mcp 0.7.1 (2026-09-21) added the second one because
      // the model picks tool arguments, so the per-call flag alone let a
      // prompt-injected call widen the SSRF guard. The prefix scrub above
      // strips every FETCH_MCP_* the box carries, so the harness sets the
      // variable itself: it starts the process AND owns the URL, which is
      // exactly the operator that switch exists for. Without this layer the
      // row read UPSTREAM from 0.7.1 on -- node refused identically, so the
      // adjudicator filed a stale harness as a broken sidecar and the call
      // never reached oam's HTTP stack.
      requires: (ctx) => ctx.loopback.error ?? null,
      env: () => ({ FETCH_MCP_ALLOW_PRIVATE_HOSTS: "1" }),
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
    // The same server with --isolated, a path the row above never takes: the
    // browser runs behind playwright-core's BrowserServer, which listens on a
    // pipe (a Windows named pipe, a Unix domain socket elsewhere) that the
    // server's own client then dials. Up to 0.17.1 oam had neither end, so
    // the server booted, listed its tools and failed every browser call
    // (#219). No user data dir: --isolated keeps the profile in memory.
    name: "pw-isolated",
    pkg: "@playwright/mcp",
    args: ["--isolated"],
    envPrefixes: ["PLAYWRIGHT_MCP_"],
    browser: true,
    call: {
      requires: (ctx) => ctx.browser.error ?? ctx.loopback.error ?? null,
      env: (ctx) => ({
        PLAYWRIGHT_MCP_BROWSER: "chromium",
        PLAYWRIGHT_MCP_EXECUTABLE_PATH: ctx.browser.path,
        PLAYWRIGHT_MCP_HEADLESS: "1",
        PLAYWRIGHT_MCP_OUTPUT_DIR: `${ctx.profileDir}-out`,
      }),
      tool: "browser_navigate",
      args: (ctx) => ({ url: `${ctx.loopback.url}page` }),
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
  {
    // The broker itself (#221). Every row above is a sidecar the broker puts
    // on oam through the npx rewrite; none of them can see a regression in
    // how oam hosts the broker -- its stdio framing, its config walk, the
    // child_process and fetch it uses to start and refresh sidecars. And
    // `yaw-mcp install` writes exactly this shape into every client config
    // when oam is installed: `<oam> run --no-check <dist/index.js>`.
    name: "yaw-mcp",
    pkg: "@yawlabs/mcp",
    oamFlags: ["--no-check"],
    // Every broker path derives from os.homedir(), and its project-config walk
    // goes from the cwd up to the root, so without this it reads the box's own
    // ~/.yaw-mcp -- the release box is a box people use.
    isolateHome: true,
    envPrefixes: ["YAW_MCP_"],
    // Nothing reaches the network or spawns unasked: no self-upgrade, no
    // sidecar refresh, no prewarm, no heal.
    env: {
      YAW_MCP_AUTO_UPGRADE: "0",
      YAW_MCP_SIDECAR_REFRESH: "0",
      YAW_MCP_AUTO_PREWARM: "0",
      YAW_MCP_AUTO_HEAL: "0",
    },
    call: {
      // Walks the config locations (the scratch home's, and the cwd up to the
      // root) and answers from what it found: no network, no sidecar started.
      tool: "mcp_connect_discover",
      args: () => ({}),
      deterministic: true,
      // The meta-tools, and nothing else: a tool from any other server means
      // the broker found a bundle outside the scratch home.
      expectTools: (tools) => {
        const missing = BROKER_META_TOOLS.filter((t) => !tools.includes(t));
        if (missing.length > 0) return `the broker no longer serves ${missing.join(", ")}`;
        const foreign = tools.filter((t) => !t.startsWith("mcp_connect_"));
        return foreign.length > 0
          ? `the broker served ${foreign.slice(0, 3).join(", ")} -- it loaded a bundle from outside its scratch home`
          : null;
      },
      expect: (text) =>
        text.includes("No servers installed")
          ? null
          : `mcp_connect_discover did not report the empty scratch home: ${text.slice(0, 120)}`,
    },
  },
];

// The meta-tools a broker with no servers installed serves: the ones a client
// needs to find, load and drop servers. It serves more (11 at @yawlabs/mcp
// 1.0.18); these are the ones whose loss would break every session.
const BROKER_META_TOOLS = [
  "mcp_connect_discover",
  "mcp_connect_activate",
  "mcp_connect_deactivate",
  "mcp_connect_dispatch",
  "mcp_connect_health",
];

// The install scripts the matrix does not run and has READ, with why skipping
// each keeps the test faithful. The gate installs with --ignore-scripts (see
// npmInstall), so it installs each sidecar a little differently from npx, which
// runs them; this table is where that difference is decided, one script at a
// time. A sidecar whose installed tree holds a script missing from it is a
// SKIP naming the script (binFor): a PASS on a tree whose install differs from
// what users get could hide an untested native path. Matched exactly on name,
// event and script text -- not on version -- so a changed script needs a fresh
// look. On 2026-09-30 this one script was the only one in the nine sidecars'
// 221 installed packages, and it did nothing under PUPPETEER_SKIP_DOWNLOAD=1;
// the trees installed with and without scripts were byte-identical.
const REVIEWED_INSTALL_SCRIPTS = [
  {
    name: "puppeteer",
    event: "postinstall",
    script: "node install.mjs",
    why: "downloads Chrome for Testing into the user cache; the matrix drives the browser already on the box, and PUPPETEER_SKIP_DOWNLOAD=1 made it a no-op before scripts were turned off",
  },
  {
    name: "@yawlabs/mcp",
    event: "preinstall",
    script: "node -e \"const major=Number(process.versions.node.split('.')[0]);if(major<20){console.error('@yawlabs/mcp requires Node 20 or newer; this is Node '+process.versions.node+'. Upgrade Node, then re-run the install.');process.exit(1);}\"",
    why: "refuses an install on node older than 20 and writes nothing; the engines floor check covers the same node, and the installed tree is the same with or without it",
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
// How long a finished probe waits for the sidecar's stderr pipe to close, so
// the last thing it -- or a process it handed off to -- printed is read.
// Normally the pipe is closed by the end of teardown, which kills whatever
// the sidecar left running; this only runs out when something outside that
// tree holds it open. Long enough for a handed-off node to start and print on
// a loaded box.
const STDERR_DRAIN_MS = 3_000;
// A process-table read, which on a loaded Windows box is mostly PowerShell
// starting up -- measured at 11-17s, so a 30s budget failed under load.
const PROCESS_TABLE_TIMEOUT_MS = 60_000;
// How long a command runBounded killed gets for its output pipes to close.
// Only something outside the killed tree holding them makes it run out.
const CLOSE_AFTER_KILL_MS = 10_000;

/** The arguments of the matrix's `npm install` of `specs` into `dir` (the
 *  stage), for `platform`.
 *
 *  Sealed: --ignore-scripts, and a script shell and a git that do not exist
 *  (NO_SCRIPT_SHELL, NO_GIT). An install then starts no process but npm (and,
 *  on Windows, the cmd.exe around npm.cmd), so the tree kill on a timeout or
 *  an interrupt reaches everything an install started. A lifecycle script can
 *  start a process that outlives its parent -- a daemon -- which is no longer
 *  under npm, so no tree kill finds it: measured, a preinstall's detached
 *  child survived the timeout's kill, went on running in the stage, and made
 *  the next rename of node_modules fail with EPERM. Which scripts that skips,
 *  and why skipping them keeps the test faithful, is REVIEWED_INSTALL_SCRIPTS.
 *
 *  On Windows npm runs through cmd.exe (shell: true), which splits its command
 *  line on spaces, so every path is quoted: unquoted, a stage under a user
 *  name with a space became two arguments. A `%` cannot be quoted past cmd.exe
 *  at all; stagePathProblem refuses such a stage before npm runs. */
function npmInstallArgs(specs, { dir = stage, platform = process.platform } = {}) {
  const quoted = (arg) => (platform === "win32" ? `"${arg}"` : arg);
  return [
    "install",
    "--no-save",
    "--no-audit",
    "--no-fund",
    "--ignore-scripts",
    quoted(`--script-shell=${join(dir, basename(NO_SCRIPT_SHELL))}`),
    quoted(`--git=${join(dir, basename(NO_GIT))}`),
    // npm 11 fetches a git dependency pinned to a full sha on GitHub as a
    // tarball, with no git at all, and under --ignore-scripts skips its
    // prepare: measured, it installed exit 0 with the dependency unbuilt,
    // where npx builds it. This refuses every git dependency (EALLOWGIT);
    // npm 10 does not know the setting, and fails closed on the refusers.
    "--allow-git=none",
    "--prefix",
    quoted(dir),
    ...specs,
  ];
}

/** Why `dir` cannot be handed to npm on `platform`, or null. cmd.exe expands
 *  %NAME% even inside quotes, and a quote in a path cannot get past it. */
function stagePathProblem(dir, platform = process.platform) {
  if (platform === "win32" && /[%"]/.test(dir)) {
    return `the stage path ${dir} holds a % or ", which cmd.exe (npm.cmd's shell) would rewrite -- point TEMP at a path without one`;
  }
  return null;
}

/** A file at a path npm would run as its script shell or its git, or null.
 *  Nothing the matrix does creates one, so one there was put there. On
 *  Windows those are `<refuser>.com` and `<refuser>.exe`: libuv tries a name
 *  with no extension only with those two added, never bare (measured: a
 *  planted `.exe` ran as npm's git). The bare path is checked everywhere --
 *  it is what runs elsewhere. */
function plantedRefuser(exists, platform = process.platform) {
  const suffixes = platform === "win32" ? ["", ".com", ".exe"] : [""];
  for (const refuser of [NO_SCRIPT_SHELL, NO_GIT]) {
    for (const suffix of suffixes) if (exists(`${refuser}${suffix}`)) return `${refuser}${suffix}`;
  }
  return null;
}

/** `npm install --no-save <specs...>` into the shared stage, sealed
 *  (npmInstallArgs) and bounded by INSTALL_TIMEOUT_MS -- and a timeout ends
 *  npm, not just its shell (runBounded). `run`, `wipe` and `exists` are seams
 *  like installAll's: production passes nothing, and the self-test hands in a
 *  timed-out or failed run, or a planted refuser, to hold what each one
 *  leaves behind. */
async function npmInstall(specs, { run = runBounded, wipe = wipeStageModules, exists = existsSync } = {}) {
  const badStage = stagePathProblem(stage);
  if (badStage) return `npm install refused: ${badStage}`;
  const planted = plantedRefuser(exists);
  if (planted) return `npm install refused: ${planted} exists, and the matrix will not hand it to npm as its script shell or its git -- remove it`;
  const r = await run(process.platform === "win32" ? "npm.cmd" : "npm", npmInstallArgs(specs), {
    timeoutMs: INSTALL_TIMEOUT_MS,
    shell: process.platform === "win32",
    // A belt: scripts are off, so puppeteer's postinstall, which downloads its
    // own Chrome, does not run. Should puppeteer ever get a reviewed path to
    // run it, the download stays off: the matrix drives the browser already
    // on the box, and a half-extracted copy an interrupted download left in
    // the user cache failed that postinstall and SKIPPED puppeteer on
    // 2026-09-12.
    env: { ...process.env, PUPPETEER_SKIP_DOWNLOAD: "1" },
  });
  if (r.status === 0) return null;
  if (r.error?.code === "ETIMEDOUT") {
    // npm was killed mid-install, so what it left can hold a package whose
    // manifest landed and whose code did not. The next install trusts that
    // manifest and installProblems cannot see the gap; both runtimes would
    // then fail to boot the sidecar, and the row read as a broken sidecar.
    // The half-written tree is moved aside, so whatever installs next starts
    // clean. (spawnSync's timeout never got here: it left npm running, which
    // finished the job while the next install and the probes used the stage.)
    const stuck = await wipe();
    const aftermath = stuck ? `; its half-written node_modules could not be moved aside (${stuck})` : "";
    return `npm install failed: ${npmFailureReason(r)}${aftermath}`;
  }
  return `npm install failed: ${npmFailureReason(r)}`;
}

// The children runBounded has running, and whether an interrupt is ending the
// matrix (endRun).
const inFlight = new Set();
let ending = false;

/** Run a command to completion, or -- once `timeoutMs` passes -- kill it and
 *  every process still under it (killTree). Resolves to spawnSync's result
 *  shape (`status`, `signal`, `stdout`, `stderr`, `error`; a timeout's error
 *  has code ETIMEDOUT), so npmFailureReason reads it unchanged. `spawnFn` is a
 *  seam for the self-test, to count what starts.
 *
 *  spawnSync's own timeout killed only the process it spawned. For npm on
 *  Windows that is the cmd.exe running npm.cmd: npm's node ran on under no one,
 *  still writing into the stage that the next install and the probes use --
 *  the kind of concurrent write that leaves hollow folders. Elsewhere it is npm
 *  itself, and the lifecycle scripts npm started ran on. So the whole tree goes.
 *  What a tree kill cannot find is a process whose parent exited before it --
 *  a daemon a lifecycle script left behind is no longer under npm. The matrix's
 *  installs run no lifecycle script (npmInstallArgs), so everything an install
 *  starts is npm, under the kill.
 *
 *  An interrupt of the matrix ends npm too: endRun kills what is in flight
 *  here before the matrix exits. npm also stays where a Ctrl-C at the terminal,
 *  or the terminal closing, reaches it directly: it shares the matrix's console
 *  and process group. It is not detached into a group of its own, which would
 *  make the tree easy to kill and put it out of the terminal's reach. And it is
 *  not spawned with windowsHide: with piped stdio, that gives cmd.exe a hidden
 *  console of its own, which no Ctrl-C and no closed window ever reaches.
 *
 *  Once the matrix is being ended, nothing here starts and nothing settles, so
 *  the run stops where it stands -- no retry of an install endRun just killed,
 *  no probe of a stage it is moving aside. And nothing starts before one turn
 *  of the event loop, which is where an interrupt is handled: one that arrived
 *  during the synchronous work before this call (the fixture setup) then finds
 *  nothing in flight and ends the run at once, instead of killing an npm
 *  started after the Ctrl-C and moving a whole, untouched install aside as
 *  half-written. */
async function runBounded(cmd, args, { timeoutMs, shell = false, env = process.env, spawnFn = spawn }) {
  await new Promise((resolveTurn) => setImmediate(resolveTurn));
  return new Promise((resolveP) => {
    if (ending) return;
    const child = spawnFn(cmd, args, { shell, env, stdio: ["ignore", "pipe", "pipe"] });
    inFlight.add(child);
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let spawnError = null;
    let settled = false;
    let guard = null;
    const settle = (status, signal) => {
      if (settled) return;
      settled = true;
      inFlight.delete(child);
      clearTimeout(timer);
      clearTimeout(guard);
      if (ending) return;
      const error = timedOut ? Object.assign(new Error(`timed out after ${timeoutMs / 1000}s`), { code: "ETIMEDOUT" }) : spawnError;
      resolveP({ status: timedOut ? null : status, signal, stdout, stderr, error });
    };
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (d) => {
      stdout += d;
    });
    child.stderr.on("data", (d) => {
      stderr += d;
    });
    child.on("error", (e) => {
      spawnError = e;
    });
    // 'close', not 'exit': the output is complete only once the pipes close.
    child.on("close", (status, signal) => settle(status, signal));
    const timer = setTimeout(async () => {
      timedOut = true;
      await killTree(child.pid);
      // Everything holding the pipes is dead, so 'close' follows at once. If
      // something outside the tree still holds them, stop waiting anyway.
      guard = setTimeout(() => settle(null, "SIGKILL"), CLOSE_AFTER_KILL_MS);
    }, timeoutMs);
  });
}

// The interrupts endRun handles. Ctrl-Break (SIGBREAK) exists only on Windows,
// and unhandled it is the worst of them: npm registers no handler for it, so
// it dies mid-install with no rollback, and the matrix dies with it, running no
// exit handler -- the half-written tree and the lock both left behind.
const INTERRUPTS = ["SIGINT", "SIGTERM", "SIGHUP", ...(process.platform === "win32" ? ["SIGBREAK"] : [])];

/** End the run on an interrupt -- Ctrl-C (SIGINT), Ctrl-Break (SIGBREAK),
 *  SIGTERM, or the terminal going away (SIGHUP) -- without leaving npm writing
 *  into the stage.
 *
 *  A Ctrl-C at the terminal reaches npm directly as well (runBounded), but npm
 *  takes an interrupt mid-install as the cue to finish the step it is on --
 *  unpacking every new package is one -- roll the install back, and only then
 *  exit (@npmcli/arborist's reify, npm 11). Measured on Windows, mid-install
 *  with a cold npm cache: npm's node was gone 2s after one Ctrl-C and still
 *  running 40s after the next -- an install still under way in the stage,
 *  which the matrix, gone at once, had left to the next run. So whatever
 *  runBounded has in flight is killed first, tree and all, and the
 *  half-written node_modules it leaves is renamed aside, as after a timeout
 *  (npmInstall) -- renamed only: deleting it is the next run's startup sweep,
 *  so the exit does not wait on a delete of a whole tree, which takes
 *  seconds. Then the stage lock is released and the matrix dies of
 *  the interrupt (dieOf). With nothing in flight, or on a second interrupt, it
 *  does that at once; a second interrupt during the rename's retries leaves
 *  what is there to the next run's damage check.
 *
 *  The note is written after the kill: when the terminal has gone away
 *  (SIGHUP), writing to it fails, and npm is dead by then.
 *
 *  `kill`, `wipe`, `release`, `die` and `note` are seams for the self-test;
 *  production passes only the signal. */
async function endRun(
  signal,
  {
    kill = killTree,
    wipe = () => wipeStageModules({ sweep: false }),
    release = () => releaseLock(),
    die = dieOf,
    note = (line) => process.stderr.write(line),
  } = {},
) {
  const running = [...inFlight];
  const again = ending;
  ending = true;
  if (!again && running.length > 0) {
    for (const child of running) await kill(child.pid);
    note(`\n  ${signal}: ended the npm install in flight; moving what it half-wrote aside\n`);
    const stuck = await wipe();
    if (stuck) note(`  could not move the stage's node_modules aside (${stuck}); the next run's damage check judges what is there\n`);
  }
  release();
  die(signal);
}

/** Die of `signal`, the way an interrupted process does, so whatever ran the
 *  matrix sees an interrupt and not an exit it could take as handled. A shell
 *  that sees its child exit normally after a Ctrl-C concludes the child dealt
 *  with it and carries on: release-local.sh went on to cut the release when
 *  endRun exited 130. On Windows that death is STATUS_CONTROL_C_EXIT, the
 *  code node's own Ctrl-C and Ctrl-Break handling ends a process with, and
 *  which Git Bash reports as a death by SIGINT. Elsewhere it is the signal
 *  itself, raised again with no listener left; that runs no 'exit' handler,
 *  so endRun releases the lock first. Should the process outlive that, it
 *  exits 128 + the signal's number.
 *  Self-contained, so the self-test can run it in a child of its own. */
function dieOf(signal) {
  if (process.platform === "win32") process.exit(0xc000013a);
  process.removeAllListeners(signal);
  process.kill(process.pid, signal);
  setTimeout(() => process.exit(128 + osConstants.signals[signal]), 2_000);
}

/** Kill `pid` and every process under it. Windows walks the tree itself
 *  (taskkill /T, from the parent-pid chain) and ends it leaves first, the root
 *  last. Elsewhere a snapshot of the tree (descendants) is killed in walk
 *  order, parents first, so a process already killed cannot start another.
 *  Either way, a process started after the tree was read is not in it. */
async function killTree(pid) {
  if (!pid) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/pid", String(pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" });
    return;
  }
  const tree = (await descendants(pid)) ?? [];
  for (const p of [pid, ...tree.map((t) => t.pid)]) {
    try {
      process.kill(p, "SIGKILL");
    } catch {
      // Already gone.
    }
  }
}

/** Why an npm run failed, from its spawnSync result.
 *
 *  npm's LAST stderr line is always "A complete log of this run can be found
 *  in: ...", so taking the tail reported a log path instead of the reason and
 *  made every SKIP undiagnosable. The first `npm error` line carries the code
 *  (E404, EACCES). Warnings are never the reason: with no error line, the
 *  first line used to be a deprecation notice, printed as the cause of an
 *  install that had actually been killed by the timeout. An install refused
 *  for wanting a script or git the matrix does not run is said in those words
 *  (refusedReason): npm's own lines are a bare ENOENT. */
function npmFailureReason(r) {
  if (r.error?.code === "ETIMEDOUT") return `timed out after ${INSTALL_TIMEOUT_MS / 1000}s`;
  if (r.error) return r.error.message;
  const refused = refusedReason(r.stderr);
  if (refused) return refused;
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

/** The reason an install was refused for wanting what the matrix does not run
 *  -- git, or a lifecycle script -- or null when that is not why it failed.
 *  npm 11 refuses a git dependency outright (EALLOWGIT, from --allow-git);
 *  otherwise either shows as a spawn of a refuser that failed, `npm error
 *  syscall spawn <path>`, and the two refusers' names differ, so the line says
 *  which. Under --ignore-scripts the script refuser is reached by a git
 *  dependency's prepare, which npm 10 runs even then, from a clone in npm's
 *  cache. A package's own install scripts never run, so they never get here:
 *  binFor reports them. Reading a package name off the `npm error path` line
 *  -- its last node_modules segment -- is a backstop, should an npm ever run
 *  one anyway. */
function refusedReason(stderr) {
  const lines = (stderr || "").split("\n").map((l) => l.trim());
  const spawned = lines.find((l) => /^npm (error|ERR!) syscall spawn /.test(l)) ?? "";
  if (spawned.includes(basename(NO_GIT)) || lines.some((l) => /^npm (error|ERR!) code EALLOWGIT$/.test(l))) {
    return "its dependency tree has a git dependency, and the matrix runs no git: it installs only what the registry serves";
  }
  if (!spawned.includes(basename(NO_SCRIPT_SHELL))) return null;
  const at = lines.find((l) => /^npm (error|ERR!) path /.test(l)) ?? "";
  if (/[\\/]_cacache[\\/]tmp[\\/]git-clone/.test(at)) {
    return "a git dependency in its tree runs a prepare script to build, and the matrix runs no lifecycle script";
  }
  const named = [...at.matchAll(/node_modules[\\/]((?:@[^\\/\s]+[\\/])?[^\\/\s]+)/g)].pop();
  const pkg = named ? named[1].replace(/\\/g, "/") : "a package";
  return `${pkg} cannot install without running a lifecycle script, and the matrix runs none`;
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
async function installAll(pkgs, install = npmInstall, note = (line) => process.stderr.write(line)) {
  const batch = await install(pkgs.map((p) => `${p}@latest`));
  if (!batch) return new Map();
  // One bad package must not take the other six down with it. Install each on
  // its own to find out WHICH one npm rejected.
  note(`  batch install failed (${batch}); retrying one by one\n`);
  const failed = new Map();
  for (const pkg of pkgs) {
    const err = await install([`${pkg}@latest`]);
    if (err) failed.set(pkg, err);
  }
  // Those solo installs pruned each other, so only the last one is still on
  // disk -- attribution is all they were for. Re-install the survivors
  // TOGETHER so they coexist for the probe loop; without this the fallback
  // would report every survivor as "not on disk after install".
  const survivors = pkgs.filter((p) => !failed.has(p));
  if (survivors.length > 0) {
    const err = await install(survivors.map((p) => `${p}@latest`));
    if (err) for (const p of survivors) failed.set(p, err);
  }
  return failed;
}

/** Install the selected sidecars, then make sure what npm left is WHOLE.
 *
 *  npm exiting 0 does not mean every required package is on disk. On
 *  2026-09-29 the shared stage held 15 hollow folders where required packages
 *  belonged, every install since had passed over them, and the matrix ran on
 *  that tree green -- only because no stdio sidecar happened to load what was
 *  missing. One that did would have failed on BOTH runtimes, and the gate
 *  would have filed an oam regression beside it as a broken sidecar. So each
 *  installed sidecar's tree is checked (installProblems). Damage gets ONE
 *  rebuild: the stage's node_modules, npm's hidden lockfile with it, is moved
 *  aside and everything reinstalled, since reinstalling over the damage is
 *  what had already failed to repair it. What is still damaged after that is
 *  returned, and its row is a SKIP that names what is missing: the matrix
 *  cannot vouch for a sidecar it could not install whole. Two runs never do
 *  this to each other's stage: the run holds the stage lock (lockStage).
 *
 *  Returns `{ installErrors, damaged, rebuilt }`: installAll's map, a map of
 *  package -> installProblems for what is still damaged, and whether the stage
 *  was rebuilt. `install`, `check` (package -> problems), `wipe` (null, or why
 *  it failed) and `note` are seams like installAll's, so the self-test can
 *  assert the sequence without npm or a disk. */
async function settleInstall(pkgs, { install = npmInstall, check = stageProblems, wipe = wipeStageModules, note = (line) => process.stderr.write(line) } = {}) {
  const damageOf = (errors) => new Map(pkgs.filter((p) => !errors.has(p)).map((p) => [p, check(p)]).filter(([, found]) => found.length > 0));
  let installErrors = await installAll(pkgs, install, note);
  let damaged = damageOf(installErrors);
  if (damaged.size === 0) return { installErrors, damaged, rebuilt: false };
  note(`  the stage's install is damaged -- ${describeDamage(damaged)}; rebuilding it from scratch\n`);
  const wipeFailed = await wipe();
  if (wipeFailed) {
    // The wipe moves node_modules aside whole or not at all, so a failure
    // should have left the tree as it was -- checked again, not assumed.
    damaged = damageOf(installErrors);
    note(`  could not move the stage's node_modules aside (${wipeFailed}); nothing was reinstalled\n`);
    return { installErrors, damaged, rebuilt: false };
  }
  installErrors = await installAll(pkgs, install, note);
  damaged = damageOf(installErrors);
  const whole = pkgs.length - damaged.size - installErrors.size;
  note(
    whole === pkgs.length
      ? "  rebuilt: every sidecar's install is now whole\n"
      : `  rebuilt: ${whole} of ${pkgs.length} sidecars reinstalled whole; the rest are SKIPs below\n`,
  );
  return { installErrors, damaged, rebuilt: true };
}

/** installProblems for an installed sidecar in the shared stage. */
function stageProblems(pkg) {
  return installProblems(join(stage, "node_modules", ...pkg.split("/")));
}

/** Move the shared stage's node_modules aside for a clean reinstall, whole or
 *  not at all: null, or why it could not be moved. A rename either happens or
 *  leaves every file where it was. A recursive delete that meets a file it
 *  cannot remove (a handle opened without delete sharing, a process whose cwd
 *  is inside) stops halfway instead -- hidden lockfile gone, packages gone, a
 *  package cut mid-delete keeping its manifest but not its code, which the
 *  next install trusts and the damage check passes. What was moved aside is
 *  then deleted best effort (sweepStageTrash) -- unless `sweep` is false, when
 *  the next run's startup sweep deletes it; a copy that will not go costs
 *  disk, not correctness. npm puts no links in the stage, and a link would be
 *  moved or removed, never followed.
 *
 *  A refusal is retried for a few seconds before it is believed. Right after
 *  an npm that ran out of time is killed, Windows has not yet let go of that
 *  process's open handles and cwd inside the tree: measured, the first rename
 *  failed with EPERM, and the same rename on the next timeout went through. */
async function wipeStageModules({ sweep = true } = {}) {
  const nm = join(stage, "node_modules");
  for (let attempt = 1; ; attempt++) {
    try {
      renameSync(nm, `${nm}.trash-${process.pid}-${Date.now()}`);
      break;
    } catch (e) {
      if (e.code === "ENOENT") return null;
      if (!["EPERM", "EBUSY", "EACCES"].includes(e.code) || attempt >= WIPE_ATTEMPTS) return e.code ?? e.message;
      await sleep(WIPE_RETRY_MS);
    }
  }
  if (sweep) await sweepStageTrash();
  return null;
}
// About 3s in all: long enough for a killed process tree's handles to close,
// short enough that a directory something really holds open is reported.
const WIPE_ATTEMPTS = 15;
const WIPE_RETRY_MS = 200;

/** Delete, best effort, every node_modules this or an earlier run moved aside.
 *
 *  Asynchronously, so the event loop keeps turning while it runs: a Ctrl-C at
 *  startup is handled at once (endRun, with nothing in flight) instead of
 *  after the whole delete, and what is left is the next run's to sweep. On a
 *  copy of the real stage (123 MB) the synchronous delete took 2.3-2.6s
 *  with the loop stalled; this one took 1.0s with the loop running.
 *
 *  Retried here, a few times and briefly, never through rm's own maxRetries:
 *  the promise rm retries at every directory level of the tree, so one file
 *  held open (a handle without delete sharing) cost 4x per level of depth --
 *  measured 13s at 3 levels and 53s at 4, where rmSync gave up in 0.6s. */
async function sweepStageTrash() {
  let names = [];
  try {
    names = readdirSync(stage).filter((name) => name.startsWith("node_modules.trash-"));
  } catch {
    return;
  }
  for (const name of names) {
    for (let attempt = 1; attempt <= SWEEP_ATTEMPTS; attempt++) {
      try {
        await rm(join(stage, name), { recursive: true, force: true });
        break;
      } catch {
        // Held open: a little later, then the next run tries again.
        if (attempt < SWEEP_ATTEMPTS) await sleep(100 * attempt);
      }
    }
  }
}
// At most 0.3s of waiting per moved-aside tree, whatever its depth.
const SWEEP_ATTEMPTS = 3;

// How long a run waits for another run to finish with the shared stage, and
// how old a lock must be before it is taken over whatever its pid says -- a
// pid can be reused by an unrelated process after a crash. The stale age has
// to outlast the longest run there can be, or a live run's lock is taken over:
// every install and every probe running out its time is about three and a half
// hours -- up to 22 installs of 300s (a batch, one by one, the survivors
// together, and all of it again after a rebuild), each timeout followed by a
// tree kill, and two probes per sidecar of up to about five minutes each. The
// self-test works it out from the constants and holds the stale age above it.
const LOCK_WAIT_MS = 15 * 60_000;
const LOCK_STALE_MS = 6 * 60 * 60_000;
// Release the stage lock this run holds, if it still holds it; set by
// lockStage once the lock is taken.
let releaseLock = () => {};

/** What to do about a stage lock another run holds: "stale" when its holder is
 *  gone or it is older than any run can last (take it over), else "wait". */
function lockVerdict({ holderAlive, ageMs }) {
  return !holderAlive || ageMs > LOCK_STALE_MS ? "stale" : "wait";
}

/** Whether a lock holder's pid is a live process. EPERM means it exists. */
function pidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === "EPERM";
  }
}

/** Hold the shared stage for the whole run: null once held, or why it could
 *  not be. Every run on the box shares one stage, and two at once had npm
 *  installing into one prefix concurrently -- a demonstrated way to leave the
 *  hollow folders and empty lockfile entries the 2026-09-29 stage had -- and
 *  would now also have one run's rebuild moving node_modules out from under
 *  the other's probes. The lock is an exclusively created file holding the
 *  pid, released at exit when it is still ours (releaseLock) -- and by endRun
 *  before an interrupt's death, which runs no exit handler on POSIX. A kill
 *  that runs no exit handler (taskkill, SIGKILL, a crash) leaves it behind, so
 *  a lock whose holder is gone, or that is older than any run, is taken over
 *  (lockVerdict); a live one is waited for, up to LOCK_WAIT_MS. */
async function lockStage(lock) {
  const mine = `${process.pid}\n`;
  const deadline = Date.now() + LOCK_WAIT_MS;
  let waiting = false;
  for (;;) {
    try {
      writeFileSync(lock, mine, { flag: "wx" });
      releaseLock = () => {
        try {
          if (readFileSync(lock, "utf8") === mine) rmSync(lock, { force: true });
        } catch {
          // Already gone.
        }
      };
      process.on("exit", () => releaseLock());
      return null;
    } catch (e) {
      if (e.code !== "EEXIST") return `cannot create the stage lock ${lock}: ${e.code ?? e.message}`;
    }
    let held;
    let ageMs;
    try {
      held = readFileSync(lock, "utf8");
      ageMs = Date.now() - statSync(lock).mtimeMs;
    } catch {
      continue; // released between the create and the read
    }
    const holder = Number.parseInt(held, 10);
    if (lockVerdict({ holderAlive: pidAlive(holder), ageMs }) === "stale") {
      // Only the lock just judged: another run may have taken it over since.
      try {
        if (readFileSync(lock, "utf8") === held) rmSync(lock, { force: true });
      } catch {
        // Gone already.
      }
      continue;
    }
    if (Date.now() > deadline) {
      return `another matrix run (pid ${holder}) has held the stage for over ${LOCK_WAIT_MS / 60_000} minutes (${lock})`;
    }
    if (!waiting) {
      process.stderr.write(`  waiting for another matrix run (pid ${holder}) to finish with the stage...\n`);
      waiting = true;
    }
    await sleep(2_000);
  }
}

/** `15 required packages missing (express, a hollow folder, required by
 *  @modelcontextprotocol/sdk@1.30.0; hono, ...; 12 more)` -- short enough for
 *  one row, each missing package named once however many trees lack it. */
function describeProblems(problems) {
  const distinct = [...new Map(problems.map((p) => [p.name, p])).values()];
  const shown = distinct
    .slice(0, 3)
    .map((p) => `${p.name}, ${p.hollow ? "a hollow folder" : "absent"}, required by ${p.from}`)
    .join("; ");
  const more = distinct.length > 3 ? `; ${distinct.length - 3} more` : "";
  return `${distinct.length} required ${distinct.length === 1 ? "package" : "packages"} missing (${shown}${more})`;
}

/** describeProblems over every damaged sidecar, led by their names. */
function describeDamage(damaged) {
  return `${[...damaged.keys()].join(", ")}: ${describeProblems([...damaged.values()].flat())}`;
}

/** What the run loop probes for `pkg`: its install error if npm failed it;
 *  a SKIP-shaped error naming what is missing if settleInstall left its tree
 *  damaged, or naming the install scripts in its tree that nobody reviewed
 *  (REVIEWED_INSTALL_SCRIPTS) -- the version still reported either way;
 *  otherwise `resolve(pkg)`. */
function binFor(pkg, installErrors, damaged, rebuilt, resolve) {
  if (installErrors.has(pkg)) return { error: installErrors.get(pkg) };
  const bin = resolve(pkg);
  if (bin.error) return bin;
  if (damaged.has(pkg)) {
    const after = rebuilt ? " even after the stage was rebuilt" : "";
    return { error: `its install is not whole${after}: ${describeProblems(damaged.get(pkg))}`, version: bin.version };
  }
  const unreviewed = unreviewedScripts(bin.scripts ?? []);
  if (unreviewed.length > 0) {
    return {
      error: `its install tree has ${unreviewed.length === 1 ? "an install script" : "install scripts"} the matrix does not run and nobody has reviewed: ${describeScripts(unreviewed)} -- read it, then list it in REVIEWED_INSTALL_SCRIPTS with why skipping it keeps the test faithful`,
      version: bin.version,
    };
  }
  return bin;
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

/** The variables os.homedir() and the per-user app-data lookups read, all
 *  pointed into `home` (isolateHome). */
function homeEnv(home) {
  return {
    HOME: home,
    USERPROFILE: home,
    APPDATA: join(home, "AppData", "Roaming"),
    LOCALAPPDATA: join(home, "AppData", "Local"),
  };
}

/** Create an isolateHome row's scratch home for one arm; returns it, as the
 *  arm's cwd. */
function makeHome(home) {
  for (const env of [homeEnv(home).APPDATA, homeEnv(home).LOCALAPPDATA]) mkdirSync(env, { recursive: true });
  return home;
}

/** The `*_RUNTIME=node` layer that pins a launcher's control arm to node. */
function nodePinFor(pin) {
  return Object.fromEntries(pin.vars.map((v) => [v, "node"]));
}

/** The environment ONE arm of a row spawns with: the scrubbed inheritance, the
 *  entry's env, the call's env (evaluated with that arm's own ctx), and on the
 *  node arm only, the launcher pin. The split is the control's whole meaning.
 *  The pin on the oam arm hands every launcher off to node, and the row
 *  compares node with node and passes whatever oam does. No pin on the node
 *  arm, and the launcher goes looking for oam and hands the control to it, so
 *  a real oam regression reads as upstream. Pure, and out of the run loop, so
 *  the self-test can hold that split. */
function armEnv(host, s, call, ctx, nodePin, inherited) {
  return sidecarEnv(
    inherited,
    s.envPrefixes ?? [],
    s.isolateHome ? homeEnv(ctx.profileDir) : {},
    s.env ?? {},
    call?.env ? call.env(ctx) : {},
    host === "node" ? nodePin : {},
  );
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

/** Adjudicate a failed oam PROBE against the node control.
 *
 * "Boot" undersells it: this is every oam probe that came back without a call
 * verdict, and a sidecar that boots and then hangs or dies while a tool call is
 * in flight lands here too -- the likeliest shape of an oam HTTP regression.
 * So the control only speaks to oam's failure when it failed at the SAME
 * point. `depth` is how far each exchange got (probe: 0 while booting, 1 + i
 * while awaiting the i-th tools/call); a hand-built verdict without one is a
 * boot failure.
 *   - node stopped EARLIER than oam: it never reached the point oam failed at,
 *     which is no evidence at all -- the rule classifyCall applies to
 *     probeFailed. oam's failure stands.
 *   - node got FURTHER than oam: node went past that point, so it is oam's.
 * Before depth was compared, any node failure read as upstream, so an oam hang
 * mid-call was excused whenever the control could not boot (an engines bump
 * past the release box's node, say), and the release went out on a warn.
 *
 * Depth alone does not close that at the SAME point. An oam boot failure met
 * by a node control that refused this box's node (an engines range anywhere
 * in the sidecar's installed tree, or a floor its launcher or a dependency
 * enforces at run time, past the node the release box runs) is two arms
 * failing at depth 0, and read as "node fails identically". `refused` is
 * nodeHostRefusal's answer for the control: when it names a reason, the
 * control ran nothing this sidecar supports, at any depth, and is no evidence.
 *
 * Split out of the run loop so it is testable without spawning a sidecar --
 * the same reason classifyCall below is a pure function. It decides whether a
 * release goes red, and it was the one verdict nothing could exercise.
 */
function classifyBoot(oam, node, refused = null) {
  const oamAt = oam.depth ?? 0;
  const nodeAt = node.depth ?? 0;
  if (!node.ok && refused) {
    return {
      state: "fail",
      why: `${oam.why}; the node control cannot run this sidecar on this box (${refused}), so nothing exonerates oam`,
    };
  }
  if (!node.ok && nodeAt < oamAt) {
    return {
      state: "fail",
      why: `${oam.why}; the node control never got that far (${node.why}), so nothing exonerates oam`,
    };
  }
  if (!node.ok && nodeAt > oamAt) {
    return {
      state: "fail",
      why: `${oam.why}; the node control got past that point before failing (${node.why}), so this is oam`,
    };
  }
  if (!node.ok) {
    const same = node.why === oam.why;
    return {
      state: "upstream",
      why: same
        ? `${oam.why}; node fails identically -- broken sidecar, not oam`
        : `${oam.why}; node also fails, differently (${node.why}) -- broken sidecar, not oam`,
    };
  }
  return {
    state: "fail",
    why: oamAt === 0
      ? `${oam.why}; the node control booted fine, so this is oam`
      : `${oam.why}; the node control answered the call, so this is oam`,
  };
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

/** The verdict on a row whose tool call was never made, before leaks.
 *  A declared `bootOnly` is coverage decided in review and does not hold the
 *  gate. An unmet `requires` is a call that SHOULD have run and could not (a
 *  loopback fixture that failed to listen, no browser on the box), so it is
 *  demoted and the run exits 3. The first version of this recorded an unmet
 *  requires as "boot" too: the run exited 0 with the gate green and the fetch
 *  assertion never run (fixed in 94d3792). */
function uncalledVerdict(s, unmet) {
  return s.bootOnly ? { state: "boot", why: s.bootOnly } : { state: "demoted", why: unmet };
}

/** The node control's evidence about the call, as classifyCall reads it. A
 *  control whose probe failed -- it could not boot, or died or hung before the
 *  call's reply -- produced no call verdict, so it is marked probeFailed, and
 *  classifyCall will not let it exonerate oam. A control that answered is its
 *  answer, a failed call included -- unless `refused` says this box's node is
 *  one the sidecar does not support. Then a failed call on node is the
 *  unsupported configuration talking, and it is marked probeFailed too. A
 *  PASS on an unsupported node is still a pass: it can only ever convict oam. */
function controlVerdict(control, refused = null) {
  const because = refused ? ` (${refused})` : "";
  if (!control.ok) return { ok: false, probeFailed: true, why: `node control could not probe: ${control.why}${because}` };
  const call = control.call ?? { ok: false, why: "node control returned no verdict" };
  if (!call.ok && refused) {
    return { ok: false, probeFailed: true, why: `node control ran outside the sidecar's supported node${because}, and its call failed: ${call.why}` };
  }
  return call;
}

/** Whether node `version` satisfies an npm `engines.node` range: true, false,
 *  or null when the range uses syntax this does not read (a hyphen range, a
 *  prerelease tag, a 0.x caret), which then decides nothing. Reads what
 *  packages declare: comparators (>= > <= < =), full and partial versions
 *  with x or * wildcards, ^ and ~, space-joined AND sets, and ||. Hand-rolled
 *  because the harness takes no dependencies; the self-test holds it to
 *  node-semver's own answers. */
function satisfiesNodeRange(range, version) {
  const have = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(String(version).trim());
  if (!have) return null;
  const v = have.slice(1, 4).map(Number);
  const cmp = (a, b) => a[0] - b[0] || a[1] - b[1] || a[2] - b[2];
  const test = (comparator) => {
    const m = /^(>=|<=|>|<|=|\^|~)?v?(\d+|[xX*])(?:\.(\d+|[xX*]))?(?:\.(\d+|[xX*]))?$/.exec(comparator);
    if (!m) return null;
    const op = m[1] ?? "=";
    const parts = [m[2], m[3], m[4]].map((p) => (p === undefined || /^[xX*]$/.test(p) ? null : Number(p)));
    // A wildcard or a missing part ends the version: 20.x.5 means 20.x.
    const given = parts.findIndex((p) => p === null);
    const n = given === -1 ? 3 : given;
    const lo = [0, 1, 2].map((i) => (i < n ? parts[i] : 0));
    // The first version past the range a partial version names: 20 -> 21.0.0, 20.1 -> 20.2.0.
    const next = (k) => [0, 1, 2].map((i) => (i < k - 1 ? lo[i] : i === k - 1 ? lo[i] + 1 : 0));
    if (n === 0) return op === "<" || op === ">" ? false : true; // * or x: any version
    switch (op) {
      case "=":
        return n === 3 ? cmp(v, lo) === 0 : cmp(v, lo) >= 0 && cmp(v, next(n)) < 0;
      case ">=":
        return cmp(v, lo) >= 0;
      case ">":
        return n === 3 ? cmp(v, lo) > 0 : cmp(v, next(n)) >= 0;
      case "<":
        return cmp(v, lo) < 0;
      case "<=":
        return n === 3 ? cmp(v, lo) <= 0 : cmp(v, next(n)) < 0;
      case "~":
        return cmp(v, lo) >= 0 && cmp(v, next(n === 1 ? 1 : 2)) < 0;
      case "^":
        if (lo[0] === 0) return null; // 0.x carets have their own rules; no node release is 0.x
        return cmp(v, lo) >= 0 && cmp(v, next(1)) < 0;
    }
    return null;
  };
  let unread = false;
  for (const set of String(range).split("||")) {
    const comparators = set.trim().replace(/(>=|<=|>|<|=|\^|~)\s+/g, "$1").split(/\s+/).filter(Boolean);
    const results = comparators.length === 0 ? [true] : comparators.map(test);
    // One comparator this cannot read leaves the whole set unread, even beside
    // a false one: node-semver rejects ">=22 garbage" outright, and a floor
    // this harness invented from half a range would convict oam on a guess.
    // A hyphen range lands here too -- its "-" is no comparator -- which keeps
    // "20 - 24" from being read as the AND set "=20 =24", false for 22.
    if (results.includes(null)) unread = true;
    else if (results.every((r) => r === true)) return true;
  }
  return unread ? null : false;
}

// The node-floor refusal a sidecar prints when the node hosting it is too old,
// in the staged sidecars' own words. fetch-mcp: "is Node 22.22.2, older than
// 24.0.0; ... needs Node 24.0.0 or newer". tailscale-mcp: "needs Node 24.0.0
// or newer, found ...". @playwright/mcp, through playwright-core's bootstrap:
// "Playwright requires Node.js 20 or higher." The launchers' OAM-floor lines
// do mention node ("to get oam 0.15.2 or newer, or launch this command with
// node", "no Node was found on PATH") but never match: the pattern needs a
// node VERSION, after "needs"/"requires" and before "or newer" and its
// synonyms, or after "is" and before ", older than".
const NODE_FLOOR_REFUSAL =
  /\b(?:needs|requires) node(?:\.js)? v?\d+(?:\.\d+){0,2} or (?:newer|later|higher|above)\b|\bis node(?:\.js)? v?\d+(?:\.\d+){0,2}, older than\b/i;

/** A package.json as an object, or null when `dir` holds none that parses. */
function readManifestAt(dir) {
  try {
    return parseManifest(readFileSync(join(dir, "package.json"), "utf8"));
  } catch {
    return null;
  }
}

/** A package.json's text as an object, or null when it does not parse. A
 *  leading byte-order mark is dropped first, as node's and npm's own readers
 *  drop it: JSON.parse throws on one, a Windows editor can write one, and a
 *  package npm installs and node loads must not read here as a hollow folder. */
function parseManifest(text) {
  try {
    return JSON.parse(text.charCodeAt(0) === 0xfeff ? text.slice(1) : text);
  } catch {
    return null;
  }
}

/** Every engines.node range the package installed at `root` runs under -- its
 *  own first, then each package of its installed dependency closure -- as
 *  `{ pkg: "name@version", range, dependency }`. Dependencies are found the
 *  way node's resolver finds them: `<dir or an ancestor>/node_modules/<name>`,
 *  nearest first, never inside a node_modules dir itself, starting from each
 *  package's own directory -- so a nested copy wins over a hoisted one, as it
 *  does at run time. dependencies, optionalDependencies and peerDependencies
 *  are followed (npm installs non-optional peers). A package with no readable
 *  manifest is skipped: an optional one npm left out, or the hollow folder a
 *  damaged install leaves behind.
 *
 *  It reads what is INSTALLED, not what loads, on purpose. A stdio sidecar
 *  never loads its SDK's HTTP-transport stack (express, hono, cors and their
 *  dependencies -- most of memory's 73 ranges and fetch's 78), but that
 *  stack is in its tree, and a floor raised there refuses the control too.
 *  That errs the way this gate errs, toward holding a release: the row names
 *  the package, and whether it loads on the failing path is the first thing to
 *  check. Measured on a fresh stage (2026-09-29): trees of 1 to 117 packages,
 *  each walked in well under a second. `readManifest` is readManifestAt,
 *  injectable so the self-test walks a tree not on disk. */
function engineRanges(root, readManifest = readManifestAt) {
  return closureFacts(root, readManifest, () => false).engines;
}

/** Every package installed for the one at `root`, itself included, handed to
 *  `visit({ at, manifest })` once each -- engineRanges' walk, which says how
 *  dependencies are found and which are followed. */
function walkClosure(root, readManifest, visit) {
  const top = readManifest(root);
  if (!top) return;
  const seen = new Set();
  const queue = [{ at: root, manifest: top }];
  while (queue.length > 0) {
    const { at, manifest } = queue.shift();
    if (seen.has(at)) continue;
    seen.add(at);
    visit({ at, manifest });
    const deps = new Set([
      ...fieldNames(manifest.dependencies),
      ...fieldNames(manifest.optionalDependencies),
      ...fieldNames(manifest.peerDependencies),
    ]);
    for (const name of deps) {
      const found = resolveInstalled(at, name, readManifest);
      if (found && !seen.has(found.at)) queue.push(found);
    }
  }
}

/** The install scripts of the package at `at`, as `{ pkg: "name@version",
 *  name, event, script }`: the ones npm runs on install -- preinstall,
 *  install, postinstall -- plus the one npm makes up, `install: node-gyp
 *  rebuild` for a package with a binding.gyp, no install or preinstall script
 *  of its own and `gypfile` not false. prepare is not an install script of an
 *  installed package. `exists` is injectable for the self-test. */
function installScriptsOf(at, manifest, exists = existsSync) {
  const pkg = `${manifest.name}@${manifest.version}`;
  const scripts = manifest.scripts && typeof manifest.scripts === "object" ? manifest.scripts : {};
  const found = ["preinstall", "install", "postinstall"]
    .filter((event) => typeof scripts[event] === "string" && scripts[event].trim() !== "")
    .map((event) => ({ pkg, name: manifest.name, event, script: scripts[event] }));
  const own = (event) => found.some((s) => s.event === event);
  if (!own("install") && !own("preinstall") && manifest.gypfile !== false && exists(join(at, "binding.gyp"))) {
    found.push({ pkg, name: manifest.name, event: "install", script: "node-gyp rebuild" });
  }
  return found;
}

/** What the run loop needs from the tree installed for the package at `root`,
 *  from ONE walk: its engines floors (engineRanges' entries) and its install
 *  scripts (installScriptsOf). */
function closureFacts(root, readManifest = readManifestAt, exists = existsSync) {
  const engines = [];
  const scripts = [];
  walkClosure(root, readManifest, ({ at, manifest }) => {
    if (typeof manifest.engines?.node === "string") {
      engines.push({ pkg: `${manifest.name}@${manifest.version}`, range: manifest.engines.node, dependency: at !== root });
    }
    scripts.push(...installScriptsOf(at, manifest, exists));
  });
  return { engines, scripts };
}

/** The scripts in `scripts` with no exact match -- name, event and script
 *  text -- in `reviewed`. */
function unreviewedScripts(scripts, reviewed = REVIEWED_INSTALL_SCRIPTS) {
  return scripts.filter((s) => !reviewed.some((r) => r.name === s.name && r.event === s.event && r.script === s.script));
}

/** Install scripts for one row: `pkg event \`script\``, two at most. */
function describeScripts(scripts) {
  const shown = scripts.slice(0, 2).map((s) => `${s.pkg} ${s.event} \`${s.script}\``).join("; ");
  return scripts.length > 2 ? `${shown}; ${scripts.length - 2} more` : shown;
}

/** The --json report's record of the install scripts a row's tree holds, all
 *  of them skipped by the sealed install: `pkg event: script`, every one. */
function skippedScriptsOf(bin) {
  return (bin.scripts ?? []).map((s) => `${s.pkg} ${s.event}: ${s.script}`);
}

/** The keys of a package.json dependency field, or none when the field is
 *  absent or not an object (a malformed manifest's string must not be read as
 *  a list of one-character names). */
function fieldNames(field) {
  return field && typeof field === "object" ? Object.keys(field) : [];
}

/** The directories node searches for package `name` required from the
 *  package at `from`, nearest first: `<from or an ancestor>/node_modules/<name>`,
 *  never inside a node_modules dir itself. */
function lookupDirs(from, name) {
  const dirs = [];
  for (let d = from; ; d = dirname(d)) {
    if (basename(d) !== "node_modules") dirs.push(join(d, "node_modules", ...name.split("/")));
    if (dirname(d) === d) return dirs;
  }
}

/** The installed package require() would load for `name` from `from` -- the
 *  first of lookupDirs holding a readable manifest -- as `{ at, manifest }`, or
 *  null. require() looks past a folder with no manifest; import does not (it
 *  stops at the first folder that exists), which installProblems reports. */
function resolveInstalled(from, name, readManifest = readManifestAt) {
  for (const at of lookupDirs(from, name)) {
    const manifest = readManifest(at);
    if (manifest) return { at, manifest };
  }
  return null;
}

/** What is damaged or missing in the install of the package at `root`, as
 *  `{ from: "name@version", name, hollow }`, found two ways -- `npm ls`'s view
 *  of the same tree, which marks both "invalid" or "missing":
 *
 *    - hollow: a folder standing FIRST in node's lookup order for a dependency
 *      that holds no readable package.json. That is damage whatever the
 *      dependency's kind, and whether or not a copy further up resolves:
 *      import stops at the first folder that exists, so a hollow nested folder
 *      shadows a whole hoisted copy, and npm never leaves one behind -- not
 *      even for an optional package it skipped. `hollow` names the folder. On
 *      2026-09-29 the shared stage had 15, mostly the SDK's HTTP stack; every
 *      reinstall since had kept them, and npm's hidden lockfile listed each
 *      as an empty entry.
 *    - absent: a REQUIRED dependency that resolves to nothing (`hollow` null).
 *      Required follows npm's own precedence (arborist loads peer, then prod,
 *      then optional edges; the last wins): a name in optionalDependencies is
 *      optional whatever else lists it, and a peer whose peerDependenciesMeta
 *      says optional -- any truthy value -- is optional too. `npm ls` calls an
 *      absent optional one UNMET OPTIONAL, which is no damage.
 *
 *  Every installed package is walked, optional ones included, since one that
 *  is there must be whole. A missing root is not this function's to report:
 *  resolveBin already says the package is not on disk. `readManifest` and
 *  `exists` are injectable for the self-test. */
function installProblems(root, readManifest = readManifestAt, exists = existsSync) {
  const top = readManifest(root);
  if (!top) return [];
  const problems = [];
  const seen = new Set();
  const queue = [{ at: root, manifest: top }];
  while (queue.length > 0) {
    const { at, manifest } = queue.shift();
    if (seen.has(at)) continue;
    seen.add(at);
    const from = `${manifest.name}@${manifest.version}`;
    const optional = new Set(fieldNames(manifest.optionalDependencies));
    const peers = fieldNames(manifest.peerDependencies);
    const peerMeta = manifest.peerDependenciesMeta && typeof manifest.peerDependenciesMeta === "object" ? manifest.peerDependenciesMeta : {};
    const required = new Set(
      [...fieldNames(manifest.dependencies), ...peers.filter((name) => !peerMeta[name]?.optional)].filter((name) => !optional.has(name)),
    );
    for (const name of new Set([...fieldNames(manifest.dependencies), ...optional, ...peers])) {
      const found = resolveInstalled(at, name, readManifest);
      if (found && !seen.has(found.at)) queue.push(found);
      const first = lookupDirs(at, name).find((dir) => exists(dir)) ?? null;
      if (first && first !== found?.at) problems.push({ from, name, hollow: first });
      else if (!found && required.has(name)) problems.push({ from, name, hollow: null });
    }
  }
  return problems;
}

/** Why the node control cannot speak for this sidecar on this box, or null.
 *  Two sources, because neither covers every sidecar. First, `engines` --
 *  engineRanges' list for the sidecar, its own range and every installed
 *  dependency's -- when this box's node (`nodeVersion`) falls outside any of
 *  them: a dependency's floor binds the control as surely as the sidecar's,
 *  and for a launcher-less sidecar (memory, puppeteer) it is the only floor
 *  there is. Second, a refusal on the control's `stderr`, for a floor
 *  enforced at run time that may be stricter than the declared ones, or
 *  declared nowhere. Today each such check equals a declared range --
 *  fetch-mcp's and tailscale-mcp's NODE_MIN and playwright-core's bootstrap
 *  check (22.19.0, 20.11.0, 20) -- so this is the backstop for a launcher whose
 *  floor moves ahead of its engines. A floor no installed package declares and
 *  none prints -- an old node that crashes a sidecar in its own words -- still
 *  reads as the sidecar's; nothing on disk says otherwise. A range this cannot
 *  read decides nothing. */
function nodeHostRefusal(engines, nodeVersion, stderr) {
  const outside = (engines ?? []).find(({ range }) => satisfiesNodeRange(range, nodeVersion) === false);
  if (outside) {
    return outside.dependency
      ? `this box's node ${nodeVersion} is outside the engines "${outside.range}" of ${outside.pkg}, installed in the sidecar's dependency tree`
      : `this box's node ${nodeVersion} is outside the sidecar's engines "${outside.range}"`;
  }
  const said = String(stderr ?? "")
    .split("\n")
    .map((l) => l.trim())
    .find((l) => NODE_FLOOR_REFUSAL.test(l));
  return said ? `it refused this box's node: ${said.length > 200 ? `${said.slice(0, 200)}...` : said}` : null;
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
//
// So are the run loop's own decisions -- which arm gets the node pin, what a
// row whose call was never made counts as, what a control that never ran is
// worth. Each once lived inline in the loop, where a mutation that flipped the
// verdict still passed every case here; they are pure functions now (armEnv,
// uncalledVerdict, controlVerdict, binFor) so they can be held, and one case
// pins the loop's calls to them at the source. So is the install the loop
// runs over: settleInstall checks it whole, and rebuilds it once, before
// anything is probed. Five cases spawn, all on node's own `-e`. Two run
// stand-in sidecars, because what they hold is probe's to report: the depth
// classifyBoot compares, and a control's stderr, including a refusal printed
// by a detached child after its parent exited (that child exits by itself).
// The third runs a stand-in install -- a node, and a child of its, under a
// shell -- and lets it time out, because the timeout must end both, not just
// the shell; it waits out that 5s bound, which is most of the self-test's run
// time. The fourth interrupts a stand-in install, which must die before the
// matrix does, and has a node die of the interrupt the way the matrix does.
// The fifth hands npm's arguments to a node spawned the way npm is, which must
// get each quoted path whole. No network, npm or oam, and no disk beyond
// reading this file. scripts/ci-local.sh runs all of it (step 13).

/** Compares by JSON shape -- the assertions here are all arrays of specs, and a
 *  printed expected-vs-actual is what makes a regression diagnosable. */
function assertDeep(actual, expected, what) {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) throw new Error(`${what}\nexpected: ${b}\nactual:   ${a}`);
}

/** Stands in for npmInstall: records every spec list it is handed, and returns
 *  npmInstall's own contract (a promise of an error string, or of null on
 *  success) -- a promise, as npmInstall's is, so a caller that forgets to
 *  await it reads every install as failed here too, not only in production.
 *  `rejects` names packages npm refuses; `rejectCall` fails the Nth call
 *  regardless, for conflicts that only exist when packages are installed
 *  together. */
function recordingInstaller({ rejects = [], rejectCall = null } = {}) {
  const calls = [];
  const install = async (specs) => {
    calls.push([...specs]);
    if (rejectCall === calls.length) {
      return "npm install failed: npm error ERESOLVE could not resolve";
    }
    const bad = specs.find((s) => rejects.includes(s.replace(/@latest$/, "")));
    return bad ? `npm install failed: npm error E404 ${bad}` : null;
  };
  return { calls, install };
}

/** Runs the self-test cases. Resolves to the process exit code. */
async function selfTest() {
  const quiet = () => {};
  // The shape of a real @yawlabs launcher's runtime switch (fetch-mcp's).
  const FETCH_LAUNCHER = [
    'const mode = (process.env.FETCH_MCP_RUNTIME ?? "auto").toLowerCase();',
    'const oam = findOam();',
    'child = spawn(oam, ["run", SERVER_ENTRY]);',
  ].join("\n");
  // The stderr each staged sidecar that enforces a node floor at run time
  // prints when node is below it, captured verbatim (2026-09-29), with the
  // line nodeHostRefusal must quote. All three exit 1 before serving.
  //   fetch-mcp 0.8.1, tailscale-mcp 0.21.0: on node 22.22.2, NODE_MIN raised
  //     to 99.0.0 in a copy of each launcher, run the way the control arm runs
  //     them (the runtime switch pinned to node).
  //   @playwright/mcp 0.0.83: the staged package untouched, on a node made to
  //     report 18.20.0 by a preload -- its floor lives in playwright-core's
  //     bootstrap, a dependency, and is 20.
  const FLOOR_REFUSALS = {
    "fetch-mcp": {
      stderr:
        "fetch-mcp: this process is Node 22.22.2, older than 99.0.0; @yawlabs/fetch-mcp needs Node 99.0.0 or newer "
        + "(the floor of its HTTP client, undici 8 -- an older Node crashes at import or on the first zstd-encoded response).\n"
        + "Install a newer Node, or install oam from https://oamjs.org and this launcher will use it.\n",
      quote: "fetch-mcp: this process is Node 22.22.2, older than 99.0.0",
    },
    "tailscale-mcp": {
      stderr:
        "tailscale-mcp: needs Node 99.0.0 or newer, found 22.22.2.\n"
        + 'Install a newer Node (https://nodejs.org/en/download), or point your MCP client\'s "command" at one.\n',
      quote: "tailscale-mcp: needs Node 99.0.0 or newer, found 22.22.2.",
    },
    "@playwright/mcp": {
      stderr: "You are running Node.js 18.20.0.\nPlaywright requires Node.js 20 or higher. \nPlease update your version of Node.js.\n",
      quote: "Playwright requires Node.js 20 or higher.",
    },
  };
  // The shape of the http_get reply @yawlabs/fetch-mcp 0.8.1 returns, captured
  // on node v22.22.2 (2026-09-29) against the fixture and against bodies the
  // call must not accept. Each came back as a 200 with isError false, so each
  // reaches the call's expect(), and that check is all that stands between the
  // reply and a PASS. What it holds is narrow: a 200 status line with its
  // reason phrase, and the marker somewhere in the reply. A body that lost or
  // mangled the marker fails; one damaged only AFTER the marker does not.
  const FETCH_REPLY_HEAD =
    "HTTP/1.1 200 OK\nURL: http://127.0.0.1:59791/\nDuration: 35ms\n\n--- Headers ---\n"
    + "connection: keep-alive\ncontent-type: application/json\ndate: Tue, 29 Sep 2026 10:41:58 GMT\n"
    + "keep-alive: timeout=5\ntransfer-encoding: chunked\n\n";
  const FETCH_REPLY_BODIES = {
    fixture: `--- Body (parsed JSON) ---\n{\n  "fixture": "${LOOPBACK_MARKER}"\n}`,
    empty: "--- Body ---\n",
    foreign: '--- Body (parsed JSON) ---\n{\n  "fixture": "somebody-else"\n}',
    // Cut halfway through the marker, whatever its length -- a fixed cut would
    // hold the whole marker once the marker got short enough, and pass. A body
    // that does not parse is rendered raw under a plain "--- Body ---".
    truncated: `--- Body ---\n{"fixture":"${LOOPBACK_MARKER.slice(0, Math.floor(LOOPBACK_MARKER.length / 2))}`,
  };
  // A stand-in sidecar on node's own `-e`: no file, no network, no oam. It
  // answers initialize and tools/list like any MCP server and serves two tools,
  // shaped like puppeteer's navigate-then-evaluate. It answers tools/calls until
  // the Nth (argv[1], default 1), and dies on that one -- what an oam crash
  // mid-call leaves behind, at whichever step it happens.
  const STAND_IN = [
    'const dieOn = Number(process.argv[1] ?? "1");',
    "let calls = 0;",
    'let buf = "";',
    'process.stdin.on("data", (d) => {',
    "  buf += d;",
    '  const lines = buf.split("\\n");',
    "  buf = lines.pop();",
    "  for (const line of lines) {",
    "    if (!line.trim()) continue;",
    "    const m = JSON.parse(line);",
    '    const reply = (result) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\\n");',
    '    if (m.method === "initialize") reply({ protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "stand-in", version: "0" } });',
    '    else if (m.method === "tools/list") reply({ tools: ["navigate", "evaluate"].map((name) => ({ name, inputSchema: { type: "object" } })) });',
    '    else if (m.method === "tools/call" && ++calls >= dieOn) process.exit(7);',
    '    else if (m.method === "tools/call") reply({ content: [{ type: "text", text: "ok" }] });',
    "  }",
    "});",
  ].join("\n");
  // Scoped and unscoped names both appear on purpose: the specs are built by
  // string concatenation, and `@scope/pkg@latest` is where that goes wrong.
  const cases = [
    {
      name: "one bad package: batch, then attribution, then a SURVIVOR RE-BATCH",
      async run() {
        const pkgs = ["@scope/alpha", "bravo", "@scope/charlie"];
        const npm = recordingInstaller({ rejects: ["bravo"] });

        const failed = await installAll(pkgs, npm.install, quiet);

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
      async run() {
        const pkgs = ["@scope/alpha", "bravo"];
        const npm = recordingInstaller();

        const failed = await installAll(pkgs, npm.install, quiet);

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
      async run() {
        const pkgs = ["alpha", "bravo"];
        const npm = recordingInstaller({ rejects: pkgs });

        const failed = await installAll(pkgs, npm.install, quiet);

        assertDeep([...failed.keys()], pkgs, "every package is reported failed");
        // With no survivors there is nothing to re-batch, and a spec-less
        // `npm install --no-save --prefix <stage>` is not a harmless no-op --
        // it re-resolves the tree over the network for no benefit.
        assertDeep(npm.calls.length, 3, "batch + 2 attributions, and no survivor re-batch");
      },
    },
    {
      name: "survivor re-batch fails: survivors are attributed, not silently dropped",
      async run() {
        const pkgs = ["alpha", "bravo", "charlie"];
        // bravo is rejected on its own; the 5th call -- the survivor re-batch --
        // is rejected too, the shape of a conflict only visible when the
        // survivors coexist.
        const npm = recordingInstaller({ rejects: ["bravo"], rejectCall: 5 });

        const failed = await installAll(pkgs, npm.install, quiet);

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
        assertDeep(
          launcherRuntimeVars(FETCH_LAUNCHER),
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
    {
      name: "a failure mid-call is not excused by a control that never booted",
      async run() {
        // One of the five cases that spawn: the depth classifyBoot compares is probe's
        // to report, so probe itself is run, against the stand-in above. The
        // call has a setup step, like puppeteer's navigate before evaluate:
        // depth has to count steps, or oam dying on the setup and node dying on
        // the asserted call would read as one point, and as upstream.
        const call = {
          tool: "evaluate",
          before: [{ tool: "navigate", args: () => ({}) }],
          args: () => ({}),
          expect: () => null,
        };
        const env = { ...process.env, NO_COLOR: "1" };
        const at = (dieOn) => probe("node", "-e", { env, scriptArgs: [STAND_IN, String(dieOn)], call, ctx: {} });
        const noBoot = await probe("node", "-e", { env, scriptArgs: ["process.exit(9)"], call, ctx: {} });
        const onSetup = await at(1);
        const onCall = await at(2);
        assertDeep(
          [noBoot.ok, noBoot.depth, onSetup.ok, onSetup.depth, onCall.ok, onCall.depth],
          [false, 0, false, 1, false, 2],
          "probe reports how far each exchange got, step by step -- the fact the verdicts below turn on",
        );
        const shallower = classifyBoot(onCall, noBoot);
        assertDeep(shallower.state, "fail", "an oam crash or hang mid-call stands when the control never reached the call");
        assertDeep(/never got that far/.test(shallower.why), true, "and the reason says why the control is no evidence");
        assertDeep(classifyBoot(onCall, onSetup).state, "fail", "a control that died on the setup step never reached the call either");
        const deeper = classifyBoot(onSetup, onCall);
        assertDeep(deeper.state, "fail", "a control that got past the step oam died on puts the failure on oam");
        assertDeep(/got past that point/.test(deeper.why), true, "and the reason says the control went further");
        assertDeep(
          classifyBoot(noBoot, onSetup).state,
          "fail",
          "a control that booted where oam could not puts the boot failure on oam",
        );
        assertDeep(
          classifyBoot(onCall, onCall).state,
          "upstream",
          "failing at the same point on both runtimes is still the sidecar's",
        );
      },
    },
    {
      name: "the node pin reaches the node arm and only the node arm",
      run() {
        const fetch = SIDECARS.find((s) => s.name === "fetch");
        const nodePin = nodePinFor(launcherRuntimeVars(FETCH_LAUNCHER));
        // The box's own copies of the switches, which must decide nothing.
        const inherited = { PATH: "/bin", FETCH_MCP_RUNTIME: "oam", FETCH_MCP_ALLOW_PRIVATE_HOSTS: "0" };
        const ctx = (host) => ({ host, loopback: { url: "http://127.0.0.1:1/" } });
        const oamEnv = armEnv("oam", fetch, fetch.call, ctx("oam"), nodePin, inherited);
        const nodeEnv = armEnv("node", fetch, fetch.call, ctx("node"), nodePin, inherited);
        assertDeep(
          nodeEnv.FETCH_MCP_RUNTIME,
          "node",
          "the control runs on node, not on whatever oam its launcher would find",
        );
        assertDeep(
          "FETCH_MCP_RUNTIME" in oamEnv,
          false,
          "the oam arm is never handed off to node, and the box's own setting is scrubbed",
        );
        for (const [k, v] of Object.entries(fetch.call.env(ctx("oam")))) {
          assertDeep([oamEnv[k], nodeEnv[k]], [v, v], `the call's ${k} reaches both arms, over the box's own value`);
        }
      },
    },
    {
      name: "a row's oam flags reach the oam arm only, before the entry",
      run() {
        assertDeep(probeArgv("oam", "e.js"), ["run", "e.js"], "no flags, no args: a bare run");
        assertDeep(probeArgv("oam", "e.js", ["serve"]), ["run", "e.js", "--", "serve"], "script args after --");
        assertDeep(
          probeArgv("oam", "e.js", [], ["--no-check"]),
          ["run", "--no-check", "e.js"],
          "the shape `yaw-mcp install` writes: the flag between run and the entry",
        );
        assertDeep(
          probeArgv("oam", "e.js", ["serve"], ["--no-check"]),
          ["run", "--no-check", "e.js", "--", "serve"],
          "flags and script args together",
        );
        assertDeep(probeArgv("node", "e.js", ["serve"], ["--no-check"]), ["e.js", "serve"], "the node arm is node's own invocation");
        const broker = SIDECARS.find((s) => s.name === "yaw-mcp");
        assertDeep(broker?.oamFlags, ["--no-check"], "the broker row launches as install writes it");
      },
    },
    {
      name: "the broker row runs in a scratch home, with nothing that reaches out switched on",
      run() {
        // #221: every broker path derives from os.homedir(), so an arm that
        // inherits the box's home reads the box's real ~/.yaw-mcp.
        const broker = SIDECARS.find((s) => s.name === "yaw-mcp");
        const inherited = {
          PATH: "/bin",
          HOME: "/home/real",
          USERPROFILE: "C:\\Users\\real",
          APPDATA: "C:\\Users\\real\\AppData\\Roaming",
          LOCALAPPDATA: "C:\\Users\\real\\AppData\\Local",
          YAW_MCP_AUTO_UPGRADE: "1",
          YAW_MCP_CONFIG: "/home/real/.yaw-mcp/bundles.json",
        };
        const nodePin = nodePinFor({ launcher: true, vars: ["YAW_MCP_DEFAULT_RUNTIME"] });
        for (const host of ["oam", "node"]) {
          const profileDir = join("scratch", `yaw-mcp-${host}`);
          const env = armEnv(host, broker, broker.call, { host, profileDir }, nodePin, inherited);
          assertDeep(
            [env.HOME, env.USERPROFILE, env.APPDATA, env.LOCALAPPDATA],
            [profileDir, profileDir, join(profileDir, "AppData", "Roaming"), join(profileDir, "AppData", "Local")],
            `the ${host} arm's home is its scratch home, not the box's`,
          );
          assertDeep(
            ["YAW_MCP_AUTO_UPGRADE", "YAW_MCP_SIDECAR_REFRESH", "YAW_MCP_AUTO_PREWARM", "YAW_MCP_AUTO_HEAL"].map((k) => env[k]),
            ["0", "0", "0", "0"],
            `the ${host} arm starts nothing and fetches nothing unasked`,
          );
          assertDeep("YAW_MCP_CONFIG" in env, false, `the box's own YAW_MCP_* settings are scrubbed from the ${host} arm`);
        }
        const fetch = SIDECARS.find((s) => s.name === "fetch");
        const fetchEnv = armEnv("oam", fetch, fetch.call, { host: "oam", loopback: { url: "x" }, profileDir: "p" }, {}, inherited);
        assertDeep(fetchEnv.HOME, "/home/real", "a row that does not ask for a scratch home keeps the box's");
      },
    },
    {
      name: "the broker row asserts on its whole tool list",
      run() {
        const { expectTools, expect } = SIDECARS.find((s) => s.name === "yaw-mcp").call;
        const served = [...BROKER_META_TOOLS, "mcp_connect_exec", "mcp_connect_secrets"];
        assertDeep(expectTools(served), null, "the meta-tools and nothing else");
        assertDeep(
          /no longer serves mcp_connect_activate/.test(expectTools(served.filter((t) => t !== "mcp_connect_activate")) ?? ""),
          true,
          "a lost meta-tool is named",
        );
        assertDeep(
          /outside its scratch home/.test(expectTools([...served, "github_create_issue"]) ?? ""),
          true,
          "a tool from a loaded server means the isolation leaked",
        );
        assertDeep(expect("No servers installed. Browse the catalog ..."), null, "the empty home's answer passes");
        assertDeep(expect("Installed servers (3): ...") !== null, true, "a populated config does not");
      },
    },
    {
      name: "a call the environment cannot support is demoted, never recorded as boot",
      run() {
        const fetch = SIDECARS.find((s) => s.name === "fetch");
        const refused = "loopback fixture server: listen EACCES: permission denied 127.0.0.1";
        assertDeep(
          fetch.call.requires({ loopback: { error: refused } }),
          refused,
          "a fixture that could not listen is an unmet requirement, with its reason",
        );
        assertDeep(fetch.call.requires({ loopback: { url: "http://127.0.0.1:1/" } }), null, "a listening one meets it");
        const v = uncalledVerdict(fetch, refused);
        assertDeep(
          v,
          { state: "demoted", why: refused },
          "recorded as boot, this exited 0 with the fetch assertion never run (fixed in 94d3792)",
        );
        assertDeep(exitCodeFor([{ state: "verified" }, v]), 3, "so the run is incomplete, not clean");
        assertDeep(
          uncalledVerdict({ bootOnly: "needs a tailnet" }, null),
          { state: "boot", why: "needs a tailnet" },
          "only a reviewed boot-only declaration skips the call without holding the gate",
        );
      },
    },
    {
      name: "a node control whose probe failed is marked unable to probe, and exonerates nothing",
      run() {
        const oamCall = { ok: false, why: "tool reported an error: fetch failed" };
        const dead = controlVerdict({ ok: false, why: "exited early (code 1) awaiting tools/list", depth: 0 });
        assertDeep(
          dead,
          { ok: false, probeFailed: true, why: "node control could not probe: exited early (code 1) awaiting tools/list" },
          "a control that never produced a call verdict is marked, not read as a failed call",
        );
        assertDeep(classifyCall(oamCall, dead, false).state, "fail", "so oam's failed call stands against it");
        const answered = controlVerdict({ ok: true, tools: ["http_get"], call: oamCall });
        assertDeep(answered, oamCall, "a control that answered is its answer, a failure included");
        assertDeep(classifyCall(oamCall, answered, false).state, "upstream", "and that answer is evidence against the sidecar");
      },
    },
    {
      name: "the fetch call holds a 200 to the fixture's own body",
      run() {
        const { expect } = SIDECARS.find((s) => s.name === "fetch").call;
        assertDeep(
          expect(FETCH_REPLY_HEAD + FETCH_REPLY_BODIES.fixture),
          null,
          "the reply fetch-mcp actually returns for the fixture passes",
        );
        for (const what of ["empty", "foreign", "truncated"]) {
          assertDeep(
            typeof expect(FETCH_REPLY_HEAD + FETCH_REPLY_BODIES[what]),
            "string",
            `a 200 with a ${what} body fails: the marker is missing, so the fixture's body did not arrive`,
          );
        }
        // fetch-mcp prints `HTTP/1.1 ${status} ${statusText}` and trims the end,
        // so a response whose reason phrase was lost reads "HTTP/1.1 200". The
        // fixture sends "OK" and node reports it; oam dropping it is a real
        // divergence, and the status check is the only thing that sees it.
        assertDeep(
          typeof expect(FETCH_REPLY_HEAD.replace("HTTP/1.1 200 OK", "HTTP/1.1 200") + FETCH_REPLY_BODIES.fixture),
          "string",
          "a 200 that lost its reason phrase fails, marker and all",
        );
      },
    },
    {
      name: "the run loop decides through the extracted functions, not inline",
      run() {
        // The cases above hold armEnv, uncalledVerdict and controlVerdict, but
        // the loop that calls them never runs offline. Handing every arm "oam",
        // skipping armEnv, or inlining a verdict again would leave every case
        // green while the release path changed -- mutation-tested, all three
        // survived. So the loop's calls are pinned at the source, the way
        // scripts/test-scripts.sh pins ci-local.sh's miri verdict functions.
        // Reads this file and nothing else. The loop is found by its opening
        // line at column 0, so this case's own copy of it, inside a string
        // above, is not mistaken for it.
        const source = readFileSync(new URL(import.meta.url), "utf8");
        const start = source.search(/^for \(const s of selected\) \{/m);
        assertDeep(start > 0, true, "the per-sidecar run loop is still where this case looks for it");
        const loop = source.slice(start).replace(/\s+/g, " ");
        const count = (needle) => loop.split(needle).length - 1;
        const calls = [
          ["const nodePin = nodePinFor(pin);", 1],
          ["const envFor = (host, call) => armEnv(host, s, call, ctxFor(host), nodePin, process.env);", 1],
          ['env: envFor("oam", call)', 1],
          ['env: envFor("node", call)', 2],
          ['const unmet = s.call?.requires ? await s.call.requires(ctxFor("oam")) : null;', 1],
          ["uncalledVerdict(s, unmet)", 1],
          // Both control sites ask whether this box's node is one the sidecar
          // runs on, and hand the answer to the verdict.
          ["const refused = nodeHostRefusal(bin.engines, process.versions.node, control.stderr);", 2],
          ["classifyBoot(oam, control, refused)", 1],
          ["const nodeVerdict = controlVerdict(control, refused);", 1],
          ["classifyCall(oam.call, nodeVerdict,", 1],
          // Every row's bin, a damaged install's SKIP included, comes from binFor.
          ["const bin = binFor(s.pkg, installErrors, damaged, rebuilt, resolveBin);", 1],
          // And every row's record of the install scripts it skipped.
          ["installScriptsSkipped: skippedScriptsOf(bin)", 1],
        ];
        assertDeep(
          calls.filter(([needle, n]) => count(needle) !== n).map(([needle, n]) => `${needle} (want ${n}, found ${count(needle)})`),
          [],
          "every arm's env, the uncalled-row verdict and the control's evidence go through the tested functions",
        );
        assertDeep(
          ["sidecarEnv(", "probeFailed: true", '? "boot"', "satisfiesNodeRange(", "NODE_FLOOR_REFUSAL", "installErrors.has("].filter(
            (inline) => loop.includes(inline),
          ),
          [],
          "and none of them is re-implemented inline beside the call",
        );
        // The install the loop runs over is settleInstall's -- checked for
        // damage and rebuilt once -- not a bare installAll. The call sits above
        // the loop, at column 0, which this case's quote of it does not.
        assertDeep(
          source.search(/^const \{ installErrors, damaged, rebuilt \} = await settleInstall\(/m) > 0,
          true,
          "the run installs through settleInstall, so a damaged stage is found and rebuilt before anything is probed",
        );
        // And the run owns the stage before touching it: the lock comes before
        // the trash sweep, the fixture wipe and the install, all at column 0.
        const lockAt = source.search(/^const lockProblem = await lockStage\(join\(stage, "\.matrix\.lock"\)\);/m);
        const sweepAt = source.search(/^await sweepStageTrash\(\);$/m);
        const fixtureAt = source.search(/^prepareFixture\(\);/m);
        const installAt = source.search(/^const \{ installErrors, damaged, rebuilt \} = await settleInstall\(/m);
        assertDeep(
          [lockAt > 0, lockAt < sweepAt, sweepAt < fixtureAt, fixtureAt < installAt],
          [true, true, true, true],
          "the stage lock is taken before anything under the stage is swept, wiped or installed",
        );
        // bin.engines and bin.scripts come from resolveBin, outside the loop.
        // Dropped there, or cut back to the sidecar's own manifest, the floors
        // of every sidecar -- or of every dependency -- would go unchecked, and
        // so would the install scripts binFor holds against the review. Its
        // body is found by its column-0 declaration, like the loop, so the
        // quotes of it in this case do not answer for it.
        const at = source.search(/^function resolveBin\(/m);
        const resolveBinBody = source.slice(at, source.indexOf("\n}\n", at)).replace(/\s+/g, " ");
        assertDeep(
          [
            "const { engines, scripts } = closureFacts(dir);",
            "return { entry, version: manifest.version, engines, scripts };",
          ].filter((needle) => at < 0 || !resolveBinBody.includes(needle)),
          [],
          "resolveBin hands the loop the engines ranges and the install scripts of each sidecar's whole installed tree",
        );
        // npm runs through runBounded, whose timeout ends npm and every process
        // under it. A spawnSync back in its place would bring back the orphan:
        // its timeout kills the shell and leaves npm writing into the stage.
        // What npmInstall does with each result is held by running it (see
        // "what a failed install leaves behind"); these are the defaults
        // production gets, which only the source shows -- and it installs with
        // npmInstallArgs, sealed, which the case "an install runs no lifecycle
        // script" holds.
        const npmAt = source.search(/^async function npmInstall\(specs, \{ run = runBounded, wipe = wipeStageModules, exists = existsSync \} = \{\}\) \{$/m);
        const npmBody = npmAt < 0 ? "" : source.slice(npmAt, source.indexOf("\n}\n", npmAt));
        assertDeep(
          [
            npmAt > 0,
            npmBody.includes("await run("),
            npmBody.includes("npmInstallArgs(specs)"),
            npmBody.includes("stagePathProblem(stage)"),
            npmBody.includes("plantedRefuser(exists)"),
            npmBody.includes("spawnSync("),
          ],
          [true, true, true, true, true, false],
          "npm installs are sealed behind the stage and refuser checks, bounded by runBounded and cleaned up by wipeStageModules, never by spawnSync's own timeout",
        );
        // And a Ctrl-C of the matrix reaches npm: no console of its own, no
        // process group of its own.
        const boundedAt = source.search(/^async function runBounded\(cmd, args, \{ timeoutMs, shell = false, env = process\.env, spawnFn = spawn \}\) \{$/m);
        const boundedBody = boundedAt < 0 ? "" : source.slice(boundedAt, source.indexOf("\n}\n", boundedAt));
        assertDeep(
          [boundedAt > 0, boundedBody.includes("spawnFn(cmd, args, {"), /windowsHide|detached/.test(boundedBody)],
          [true, true, false],
          "runBounded's child shares the matrix's console and process group",
        );
        // And an interrupt, from the moment the stage is held until the run
        // is done with it, ends npm first and releases the lock (endRun) --
        // with production's own kill, rename, release and death, which the
        // interrupt case replaces and only the source shows. (lockAt and
        // installAt as found above.)
        const signalsAt = source.search(/^for \(const signal of INTERRUPTS\) process\.on\(signal, \(\) => endRun\(signal\)\);$/m);
        // Every interrupt this platform has -- Ctrl-Break only on Windows,
        // where unhandled it kills npm and the matrix with no cleanup at all.
        assertDeep(
          INTERRUPTS,
          process.platform === "win32" ? ["SIGINT", "SIGTERM", "SIGHUP", "SIGBREAK"] : ["SIGINT", "SIGTERM", "SIGHUP"],
          "endRun handles Ctrl-C, SIGTERM, a terminal gone away and, on Windows, Ctrl-Break",
        );
        // The startup sweep runs once an interrupt is handled, and deletes
        // asynchronously, so a Ctrl-C during it is handled at once; a
        // synchronous delete held the event loop, and the Ctrl-C, for seconds.
        // And never with rm's own maxRetries, which retries at every level of
        // the tree: one held-open file cost 4x per level of depth.
        const sweepFnAt = source.search(/^async function sweepStageTrash\(\) \{$/m);
        const sweepBody = sweepFnAt < 0 ? "" : source.slice(sweepFnAt, source.indexOf("\n}\n", sweepFnAt));
        assertDeep(
          [sweepFnAt > 0, sweepBody.includes("await rm("), sweepBody.includes("rmSync("), sweepBody.includes("maxRetries"), sweepAt > signalsAt],
          [true, true, false, false, true],
          "the startup sweep deletes without blocking the event loop or retrying per directory, after the interrupt handlers are in place",
        );
        const guardAt = source.search(/^process\.stderr\.on\("error", \(\) => \{\}\);$/m);
        // endRun's head is its signature up to the `) {` that opens its body,
        // inside endRun: a close not found there is no match, not a slice that
        // runs on into this case's own quote of the defaults.
        const endAt = source.search(/^async function endRun\(/m);
        const endBodyEnd = endAt < 0 ? -1 : source.indexOf("\n}\n", endAt);
        const endClose = endAt < 0 ? -1 : source.indexOf("\n) {\n", endAt);
        const endHead = endClose < 0 || endClose > endBodyEnd ? "" : source.slice(endAt, endClose).replace(/\s+/g, " ");
        assertDeep(
          [lockAt > 0, guardAt > lockAt, signalsAt > guardAt, installAt > signalsAt],
          [true, true, true, true],
          "an interrupt once the stage is held goes through endRun, installs included, and a failed write to a terminal gone away cannot cut it short",
        );
        assertDeep(
          endHead.includes(
            "kill = killTree, wipe = () => wipeStageModules({ sweep: false }), release = () => releaseLock(), die = dieOf, note = (line) => process.stderr.write(line),",
          ),
          true,
          "an interrupt kills npm's whole tree, renames what it half-wrote aside, releases the lock and dies of the signal",
        );
      },
    },
    {
      name: "a node control that cannot run the sidecar on this box's node exonerates nothing",
      async run() {
        // The gap depth could not close: the sidecar's floor moves past the
        // release box's node, the control refuses at boot, and an oam boot
        // failure beside it -- also depth 0 -- read as "node fails identically".
        const oamBoot = { ok: false, why: "exited early (code 1) awaiting tools/list", depth: 0 };
        const nodeBoot = { ok: false, why: "exited early (code 1) awaiting tools/list", depth: 0 };
        const refused = 'this box\'s node 22.22.2 is outside the sidecar\'s engines ">=24"';
        assertDeep(classifyBoot(oamBoot, nodeBoot).state, "upstream", "with nothing refused, failing alike at boot is still the sidecar's");
        const v = classifyBoot(oamBoot, nodeBoot, refused);
        assertDeep(v.state, "fail", "a control this box's node cannot run is no evidence, so oam's boot failure is oam's");
        assertDeep(v.why.includes(refused), true, "and the row says why the control does not count");
        // The call path. A failed call on an unsupported node is the
        // unsupported node talking; a pass there can still convict oam.
        const oamCall = { ok: false, why: "tool reported an error: fetch failed" };
        const failedThere = controlVerdict({ ok: true, tools: ["http_get"], call: oamCall }, refused);
        assertDeep(failedThere.probeFailed, true, "a control's failed call on an unsupported node is marked, not read as the sidecar's bug");
        assertDeep(classifyCall(oamCall, failedThere, false).state, "fail", "so oam's failed call stands against it");
        const passedThere = controlVerdict({ ok: true, tools: ["http_get"], call: { ok: true, text: "t" } }, refused);
        assertDeep(passedThere, { ok: true, text: "t" }, "a pass on an unsupported node is still a pass");
        assertDeep(classifyCall(oamCall, passedThere, false).state, "fail", "and it still convicts an oam that failed");
        // End to end through probe: a control that prints a launcher's real
        // refusal and exits, the way tailscale-mcp does, has that refusal read
        // off its stderr -- including when it exits the moment it has printed.
        const env = { ...process.env, NO_COLOR: "1" };
        const say = FLOOR_REFUSALS["tailscale-mcp"].stderr;
        const refusing = await probe("node", "-e", {
          env,
          scriptArgs: [`process.stderr.write(${JSON.stringify(say)}); process.exit(1);`],
          call: null,
          ctx: {},
        });
        const found = nodeHostRefusal(null, "22.22.2", refusing.stderr);
        assertDeep([refusing.ok, refusing.depth, refusing.stderr === say], [false, 0, true], "probe keeps all of the control's stderr");
        assertDeep(classifyBoot(oamBoot, refusing, found).state, "fail", "and the refusal in it keeps oam's boot failure oam's");
        // A launcher that hands off exits first, and the refusal comes from the
        // process it started, after 'exit' has already fired. Detached, so it
        // outlives its parent's job object on Windows, and it prints the
        // moment it starts -- which is after its parent is gone. What this
        // holds is that probe waits for the pipe to close, not how long:
        // under STDERR_DRAIN_MS the case once missed the refusal on a loaded
        // box (one run in ten), cause not established -- the hand-off itself
        // measured under 1s to print even beside 24 CPU burners. The cap is
        // the only timing in the case, so a long one takes timing out of it,
        // and it costs nothing when the case passes: the pipe closes the
        // moment the child exits.
        const late = `process.stderr.write(${JSON.stringify(say)});`;
        const handOff =
          'require("node:child_process").spawn(process.execPath, ["-e", '
          + `${JSON.stringify(late)}], { detached: true, stdio: ["ignore", "ignore", "inherit"] }); process.exit(1);`;
        const handedOff = await probe("node", "-e", { env, scriptArgs: [handOff], call: null, ctx: {}, drainMs: 60_000 });
        assertDeep(
          [handedOff.ok, typeof nodeHostRefusal(null, "22.22.2", handedOff.stderr)],
          [false, "string"],
          "a refusal printed after the launcher exited is still read -- probe waits for stderr to close",
        );
      },
    },
    {
      name: "a node-floor refusal is read from engines and from the launchers' own words",
      run() {
        // Every distinct engines.node range declared anywhere in the nine
        // staged sidecars' installed trees admits node 22.22.2, the release
        // box's node -- so nothing is refused today, and no passing row can
        // change. (2026-09-29, by engineRanges over a FRESH stage -- 221
        // packages, none hollow: 41 distinct ranges from 225 declarations, in
        // trees of 332 packages all told. A damaged stage, missing the SDK's
        // HTTP stack, had shown 27.)
        const today = [
          ">=18", ">=20", ">= 0.6", ">= 0.10", ">= 8", ">=18.0.0", ">= 18", ">= 16",
          ">=16.9.0", ">=16.20.0", ">=0.10.0", ">= 0.8", ">=8", ">=6.6.0", ">=6.0", ">= 18.0.0",
          ">=0.6", ">= 12", ">= 0.4", ">=22.19.0", ">=14.0.0", ">=16.0.0", ">=20.11.0", ">=22",
          ">=14", ">= 10.17.0", ">=0.4.0", ">= 14", ">=12", ">=10", ">=6", ">=10.0.0",
          "6.* || 8.* || >= 10.*", ">=4", ">=6.9.0", "*", ">= 10.0.0", ">= 0.4.0", ">= 6.0.0", ">=7.0.0",
          ">=4.0",
        ];
        const dep = (range) => ({ pkg: "some-dep@1.0.0", range, dependency: true });
        assertDeep(today.filter((range) => nodeHostRefusal([dep(range)], "22.22.2", "")), [], "today's floors refuse nothing");
        assertDeep(nodeHostRefusal(null, "22.22.2", ""), null, "a sidecar with no ranges and no refusal is not refused");
        const own = nodeHostRefusal([{ pkg: "s@1.0.0", range: ">=24", dependency: false }], "22.22.2", "");
        assertDeep(typeof own === "string" && own.includes("the sidecar's engines"), true, "a sidecar's own floor past this box's node is a refusal");
        const theirs = nodeHostRefusal(
          [{ pkg: "s@1.0.0", range: ">=18", dependency: false }, { pkg: "playwright-core@1.64.0", range: ">=24", dependency: true }],
          "22.22.2",
          "",
        );
        assertDeep(
          typeof theirs === "string" && theirs.includes("playwright-core@1.64.0"),
          true,
          "so is a dependency's, and the row names the dependency",
        );
        assertDeep(nodeHostRefusal([dep("20 - 24")], "22.22.2", ""), null, "a range this cannot read decides nothing");
        for (const [who, { stderr, quote }] of Object.entries(FLOOR_REFUSALS)) {
          const r = nodeHostRefusal(null, "22.22.2", `some earlier log line\n${stderr}`);
          assertDeep(typeof r === "string" && r.includes(quote), true, `${who}'s own refusal is found, and quoted`);
        }
        // The OAM-floor refusals the same launchers print must never read as
        // node's (rendered from their templates at 0.15.0 against 0.15.2),
        // including the ones that mention node by name.
        for (const said of [
          "fetch-mcp: this process is oam 0.15.0, older than 0.15.2, and no newer oam was found",
          "fetch-mcp: this process is oam 0.15.0, older than 0.15.2, and no Node was found on PATH to run the server.",
          "Run `oam self-update` to get oam 0.15.2 or newer, or launch this command with node.",
          "Put Node on PATH, or launch this command with node.",
          "redis-mcp: REDIS_MCP_RUNTIME=oam but no usable oam (0.15.2 or newer) was found.",
        ]) {
          assertDeep(nodeHostRefusal(null, "22.22.2", said), null, `not a node refusal: ${said}`);
        }
      },
    },
    {
      name: "a sidecar's installed dependencies are walked the way node resolves them",
      run() {
        // A tree that is not on disk, shaped like npm's: a nested copy under
        // the package that needs it, the rest hoisted, one dependency never
        // installed, an optional one installed and one npm left out, a peer,
        // and a cycle.
        const tree = new Map();
        const nm = join("/", "stage", "node_modules");
        const put = (parts, manifest) => tree.set(join(nm, ...parts), manifest);
        put(["app"], {
          name: "app",
          version: "1.0.0",
          engines: { node: ">=18" },
          dependencies: { lib: "^1", "@s/core": "^2", gone: "^1" },
          optionalDependencies: { native: "^1", unbuilt: "^1" },
          peerDependencies: { peer: "*" },
        });
        put(["native"], { name: "native", version: "1.2.0", engines: { node: ">=12" } });
        put(["app", "node_modules", "lib"], { name: "lib", version: "1.0.0", engines: { node: ">=20" }, dependencies: { app: "^1" } });
        // A hoisted lib nobody here resolves to: node finds app's nested copy first.
        put(["lib"], { name: "lib", version: "2.0.0", engines: { node: ">=99" } });
        put(["@s", "core"], { name: "@s/core", version: "2.0.0", dependencies: { deep: "^1" } });
        put(["deep"], { name: "deep", version: "1.0.0", engines: { node: ">=16" }, dependencies: "not-an-object" });
        // What a malformed string field would reach if it were read as names:
        // Object.keys("not-an-object") is "0".."12", and "0" is installed here,
        // declaring a floor past every node. The walk must not follow it.
        put(["0"], { name: "zero", version: "1.0.0", engines: { node: ">=99" } });
        put(["peer"], { name: "peer", version: "3.0.0", engines: { node: ">=14" } });
        const read = (dir) => tree.get(dir) ?? null;
        const ranges = engineRanges(join(nm, "app"), read);
        assertDeep(
          ranges,
          [
            { pkg: "app@1.0.0", range: ">=18", dependency: false },
            { pkg: "lib@1.0.0", range: ">=20", dependency: true },
            { pkg: "native@1.2.0", range: ">=12", dependency: true },
            { pkg: "peer@3.0.0", range: ">=14", dependency: true },
            { pkg: "deep@1.0.0", range: ">=16", dependency: true },
          ],
          "own range first, then every installed dependency, optional and peer included -- the nested copy, not the hoisted one; missing ones skipped; the cycle walked once",
        );
        assertDeep(nodeHostRefusal(ranges, "22.22.2", ""), null, "a tree whose floors all admit this node refuses nothing");
        const refused = nodeHostRefusal(ranges, "19.0.0", "");
        assertDeep(
          typeof refused === "string" && refused.includes("lib@1.0.0") && refused.includes(">=20"),
          true,
          "a dependency's floor above this node refuses the control, and names the dependency",
        );
        assertDeep(engineRanges(join(nm, "absent"), read), [], "a package not on disk has no ranges");
      },
    },
    {
      name: "a required package missing from a sidecar's install is found, hollow or absent",
      run() {
        // An npm-shaped tree with the damage the shared stage had on
        // 2026-09-29 -- a hollow folder where a required package belongs --
        // beside the absences npm is allowed, and ones it is not.
        const tree = new Map();
        const nm = join("/", "stage", "node_modules");
        const put = (parts, manifest) => tree.set(join(nm, ...parts), manifest);
        // Folders with no package.json: where a required package belongs, where
        // an optional one does, and a nested one shadowing a whole hoisted copy.
        const hollow = new Set([join(nm, "express"), join(nm, "hollowopt"), join(nm, "sidecar", "node_modules", "shadowed")]);
        put(["sidecar"], {
          name: "sidecar",
          version: "1.0.0",
          dependencies: { sdk: "^1", express: "^5", gone: "^1", maybe: "^1", shadowed: "^1" },
          // "maybe" is in dependencies too, and "fsev" is a peer too: npm treats
          // both as optional, since the optional edge loads last and wins.
          optionalDependencies: { maybe: "^1", native: "^1", hollowopt: "^1", fsev: "^2" },
          // zod installed; react a required peer npm did not install; ts
          // optional; "weird" optional by a truthy meta value that is not true.
          peerDependencies: { zod: "*", react: "*", ts: "*", fsev: "*", weird: "*" },
          peerDependenciesMeta: { ts: { optional: true }, weird: { optional: "true" } },
        });
        put(["sdk"], { name: "sdk", version: "1.30.0", dependencies: { cors: "^2" } });
        // An optional package that IS installed must be whole too.
        put(["native"], { name: "native", version: "1.0.0", dependencies: { "node-gyp-build": "^4" } });
        put(["zod"], { name: "zod", version: "3.0.0" });
        put(["shadowed"], { name: "shadowed", version: "2.0.0" });
        const read = (dir) => tree.get(dir) ?? null;
        const exists = (dir) => tree.has(dir) || hollow.has(dir);
        assertDeep(
          installProblems(join(nm, "sidecar"), read, exists),
          [
            { from: "sidecar@1.0.0", name: "express", hollow: join(nm, "express") },
            { from: "sidecar@1.0.0", name: "gone", hollow: null },
            // require() would look past it to the hoisted copy; import stops here.
            { from: "sidecar@1.0.0", name: "shadowed", hollow: join(nm, "sidecar", "node_modules", "shadowed") },
            // npm never leaves a folder for an optional package it skipped.
            { from: "sidecar@1.0.0", name: "hollowopt", hollow: join(nm, "hollowopt") },
            { from: "sidecar@1.0.0", name: "react", hollow: null },
            { from: "sdk@1.30.0", name: "cors", hollow: null },
            { from: "native@1.0.0", name: "node-gyp-build", hollow: null },
          ],
          "every hollow folder first in the lookup order, and every absent required package, anywhere in the tree; absent optional ones and optional peers excused as npm excuses them",
        );
        assertDeep(installProblems(join(nm, "zod"), read, exists), [], "a whole tree has no problems");
        assertDeep(installProblems(join(nm, "absent"), read, exists), [], "a package not on disk is resolveBin's to report");
        // A manifest with a byte-order mark is a manifest: node and npm drop
        // the mark, and so must the reader, or a working package reads as hollow.
        assertDeep(parseManifest('﻿{"name":"bom","version":"1.0.0"}'), { name: "bom", version: "1.0.0" }, "a leading BOM is dropped");
        assertDeep(parseManifest('{"name": '), null, "and one that does not parse is null");
      },
    },
    {
      name: "a damaged install gets one rebuild, and what stays damaged is a SKIP that names it",
      async run() {
        const damage = [{ from: "sdk@1.30.0", name: "express", hollow: join("/", "stage", "node_modules", "express") }];
        // install, check and wipe recorded in one sequence; `healed` is when
        // check stops finding damage (after the wipe, never, or at once).
        // `reject` is refused on every install; `rejectAfterWipe` only by the
        // rebuild's -- so what the rebuild returns has to be the rebuild's own.
        const run = async ({ healed, wipeFails = null, reject = [], rejectAfterWipe = [] }) => {
          const events = [];
          const notes = [];
          let wiped = false;
          // install and wipe return promises, as npmInstall and
          // wipeStageModules do, so a missing await fails here as it would there.
          const result = await settleInstall(["good", ...reject, ...rejectAfterWipe], {
            install: async (specs) => {
              events.push(`install ${specs.join(" ")}`);
              const refusing = wiped ? [...reject, ...rejectAfterWipe] : reject;
              const bad = specs.find((s) => refusing.includes(s.replace(/@latest$/, "")));
              return bad ? `npm install failed: npm error E404 ${bad}` : null;
            },
            check: (pkg) => {
              events.push(`check ${pkg}`);
              return healed === "now" || (healed === "after-wipe" && wiped) ? [] : damage;
            },
            wipe: async () => {
              events.push("wipe");
              wiped = wipeFails === null;
              return wipeFails;
            },
            note: (line) => notes.push(line),
          });
          // Maps print as {} through assertDeep's JSON, so both become objects.
          return {
            events,
            notes,
            installErrors: Object.fromEntries(result.installErrors),
            damaged: Object.fromEntries(result.damaged),
            rebuilt: result.rebuilt,
          };
        };
        const refused = "npm install failed: npm error E404 bad@latest";
        assertDeep(
          await run({ healed: "now" }),
          { events: ["install good@latest", "check good"], notes: [], installErrors: {}, damaged: {}, rebuilt: false },
          "a whole install is checked once and left alone",
        );
        const healedRun = await run({ healed: "after-wipe" });
        assertDeep(
          [healedRun.events, healedRun.installErrors, healedRun.damaged, healedRun.rebuilt, healedRun.notes.at(-1)],
          [
            ["install good@latest", "check good", "wipe", "install good@latest", "check good"],
            {},
            {},
            true,
            "  rebuilt: every sidecar's install is now whole\n",
          ],
          "a damaged one is moved aside, reinstalled and checked again -- once -- and what the rebuild healed is not skipped",
        );
        const stuck = await run({ healed: "never" });
        assertDeep(
          [stuck.rebuilt, stuck.damaged, stuck.notes.at(-1)],
          [true, { good: damage }, "  rebuilt: 0 of 1 sidecars reinstalled whole; the rest are SKIPs below\n"],
          "damage that survives the rebuild is returned, and said, not hidden",
        );
        const locked = await run({ healed: "never", wipeFails: "EBUSY" });
        assertDeep(
          [locked.events, locked.rebuilt, Object.keys(locked.damaged), /aside \(EBUSY\); nothing was reinstalled/.test(locked.notes.at(-1))],
          [["install good@latest", "check good", "wipe", "check good"], false, ["good"], true],
          "a stage that cannot be moved aside is not reinstalled over, and is checked again rather than assumed unchanged",
        );
        const refusedOnly = await run({ healed: "now", reject: ["bad"] });
        assertDeep(
          [refusedOnly.installErrors, refusedOnly.damaged, refusedOnly.events.filter((e) => e.startsWith("check"))],
          [{ bad: refused }, {}, ["check good"]],
          "a package npm refused keeps npm's reason, and is not checked for damage",
        );
        const both = await run({ healed: "after-wipe", reject: ["bad"] });
        assertDeep(
          [both.installErrors, both.damaged, both.rebuilt, both.notes.at(-1), both.notes.some((n) => /now whole/.test(n))],
          [{ bad: refused }, {}, true, "  rebuilt: 1 of 2 sidecars reinstalled whole; the rest are SKIPs below\n", false],
          "a rebuild that npm refuses part of says so, and keeps the refusal -- never 'every sidecar's install is now whole'",
        );
        const lateRefusal = await run({ healed: "after-wipe", rejectAfterWipe: ["bad"] });
        assertDeep(
          [lateRefusal.installErrors, lateRefusal.damaged, lateRefusal.notes.at(-1)],
          [{ bad: refused }, {}, "  rebuilt: 1 of 2 sidecars reinstalled whole; the rest are SKIPs below\n"],
          "what the rebuild's own install refused is reported, not the first install's clean slate",
        );
        // The stage lock: a holder that is gone, or a lock older than any run
        // can last, is taken over; a live, recent one is waited for.
        assertDeep(
          [
            lockVerdict({ holderAlive: false, ageMs: 1_000 }),
            lockVerdict({ holderAlive: true, ageMs: 60_000 }),
            lockVerdict({ holderAlive: true, ageMs: LOCK_STALE_MS + 1 }),
          ],
          ["stale", "wait", "stale"],
          "a dead or ancient lock is taken over; a live one is waited for",
        );
        // And "ancient" is older than any live run: every install and every
        // probe running out its time. installAll makes a batch install, one per
        // sidecar and one of the survivors, and settleInstall runs it twice
        // when it rebuilds; a timed-out install then waits on the tree kill's
        // table read, the pipes and the wipe's retries. Each sidecar gets two
        // probes, each booting, making every call and tearing down to the last
        // wait.
        const perInstall = INSTALL_TIMEOUT_MS + PROCESS_TABLE_TIMEOUT_MS + CLOSE_AFTER_KILL_MS + WIPE_ATTEMPTS * WIPE_RETRY_MS;
        const perProbe = (s) =>
          BOOT_TIMEOUT_MS
          + CALL_TIMEOUT_MS * (s.call ? 1 + (s.call.before?.length ?? 0) : 0)
          + PROCESS_TABLE_TIMEOUT_MS
          + 2 * EXIT_GRACE_MS
          + LEAK_SETTLE_MS
          + STDERR_DRAIN_MS;
        const longestRun = 2 * (SIDECARS.length + 2) * perInstall + SIDECARS.reduce((sum, s) => sum + 2 * perProbe(s), 0);
        assertDeep(
          LOCK_STALE_MS > longestRun,
          true,
          `a lock older than the longest run there can be (${Math.round(longestRun / 60_000)} min) is never a live run's`,
        );
        assertDeep([pidAlive(process.pid), pidAlive(0), pidAlive(Number.NaN)], [true, false, false], "this process is alive; no pid is not");
        // The row. An install error wins; damage is a SKIP naming what is
        // missing, with the version still shown; otherwise the bin as resolved.
        const resolve = (pkg) => ({ entry: `/bin/${pkg}`, version: "1.0.0" });
        const none = new Map();
        assertDeep(binFor("a", new Map([["a", "npm install failed: E404"]]), none, false, resolve), { error: "npm install failed: E404" }, "an install error wins");
        assertDeep(binFor("a", none, none, false, resolve), { entry: "/bin/a", version: "1.0.0" }, "a whole install resolves as before");
        const skipped = binFor("a", none, new Map([["a", damage]]), true, resolve);
        assertDeep(
          [skipped.version, /even after the stage was rebuilt: 1 required package missing \(express, a hollow folder, required by sdk@1\.30\.0\)/.test(skipped.error)],
          ["1.0.0", true],
          "a damaged install is a SKIP that names the missing package, where it was, and who needed it",
        );
        assertDeep(exitCodeFor([{ state: "verified" }, { state: "skip" }]), 3, "which leaves the run incomplete, never clean");
      },
    },
    {
      name: "an install that runs out of time is ended with every process under it",
      async run() {
        // npm's shape: a shell (on Windows, the cmd.exe running npm.cmd), npm's
        // node under it, and a lifecycle script under that, holding npm's
        // output pipes. Killing only the shell -- what spawnSync's timeout did
        // -- leaves the rest running. The stand-in node starts a child that
        // never exits, prints both pids, and never exits itself; the bound must
        // end BOTH. Two levels under the shell, because a shell may exec a lone
        // command instead of forking it: bash does, so where /bin/sh is bash
        // the stand-in IS the spawned process, and only its child still needs
        // the tree walk. Quoted by hand: a shell command line is one string, so
        // the stand-in holds no double quote, $ or backquote.
        const standIn =
          "var c = require('child_process').spawn(process.execPath, ['-e', 'setInterval(function () {}, 1000)'], { stdio: ['ignore', 'inherit', 'inherit'] });"
          + " process.stdout.write(process.pid + ' ' + c.pid); setInterval(function () {}, 1000)";
        const r = await runBounded(`"${process.execPath}"`, ["-e", `"${standIn}"`], { timeoutMs: 5_000, shell: true });
        const pids = r.stdout.trim().split(" ").map((s) => Number.parseInt(s, 10));
        const printed = pids.length === 2 && pids.every(Number.isInteger);
        let left = pids.filter((pid) => Number.isInteger(pid) && isAlive(pid));
        for (let waited = 0; left.length > 0 && waited < 3_000; waited += 100) {
          await sleep(100);
          left = left.filter(isAlive);
        }
        for (const pid of left) {
          try {
            process.kill(pid); // leave nothing behind, even when the case fails
          } catch {
            // Already gone.
          }
        }
        assertDeep(
          [r.error?.code, r.status, printed, left.length],
          ["ETIMEDOUT", null, true, 0],
          "the timeout ends the node under the shell and the child under that, not just the shell -- nothing is left writing into the stage -- and reports no exit status npmInstall could read as success",
        );
        assertDeep(npmFailureReason(r), `timed out after ${INSTALL_TIMEOUT_MS / 1000}s`, "and the install is reported as timed out");
        // A run that ends on its own reads as it always did.
        const done = await runBounded(process.execPath, ["-e", "process.stdout.write('done'); process.exitCode = 3"], { timeoutMs: 30_000 });
        assertDeep([done.status, done.stdout, done.error], [3, "done", null], "a finished run keeps its status and output, and no timeout");
      },
    },
    {
      name: "what a failed install leaves behind",
      async run() {
        // npmInstall run on runBounded's result shapes, with its run and wipe
        // replaced: which result moves the stage's node_modules aside is the
        // decision, and a source check could not tell a wipe inside the
        // timeout's branch from one after it, or after its return.
        const install = async (result, wipeSays = null, exists = () => false) => {
          const calls = [];
          let wipes = 0;
          const said = await npmInstall(["a@latest", "b@latest"], {
            run: async (cmd, args, opts) => {
              calls.push({ cmd, args, opts });
              return result;
            },
            wipe: async () => {
              wipes++;
              return wipeSays;
            },
            exists,
          });
          return { said, calls, wipes };
        };
        const ran = (status, stderr = "", error = null) => ({ status, signal: null, stdout: "", stderr, error });
        const timedOut = ran(null, "", Object.assign(new Error("timed out after 300s"), { code: "ETIMEDOUT" }));

        const ok = await install(ran(0));
        assertDeep([ok.said, ok.wipes], [null, 0], "a finished install is no error, and the stage is left as it is");
        const { cmd, args, opts } = ok.calls[0];
        assertDeep(
          [ok.calls.length, cmd, args, opts.timeoutMs, opts.shell, opts.env.PUPPETEER_SKIP_DOWNLOAD],
          [
            1,
            process.platform === "win32" ? "npm.cmd" : "npm",
            npmInstallArgs(["a@latest", "b@latest"]),
            INSTALL_TIMEOUT_MS,
            process.platform === "win32",
            "1",
          ],
          "npm installs the specs into the stage, once, sealed, bounded by INSTALL_TIMEOUT_MS, without puppeteer's Chrome download",
        );
        // A refuser that exists would be run by npm as its script shell or
        // git: the install is refused, and npm never starts.
        for (const planted of [NO_SCRIPT_SHELL, NO_GIT]) {
          const refused = await install(ran(0), null, (path) => path === planted);
          assertDeep(
            [refused.calls.length, refused.said?.startsWith(`npm install refused: ${planted} exists`)],
            [0, true],
            `an install whose ${basename(planted)} exists is refused before npm runs`,
          );
        }

        const killed = await install(timedOut);
        assertDeep(
          [killed.said, killed.wipes],
          [`npm install failed: timed out after ${INSTALL_TIMEOUT_MS / 1000}s`, 1],
          "an install killed by its timeout moves its half-written node_modules aside before it reports",
        );
        const stuck = await install(timedOut, "EPERM");
        assertDeep(
          [stuck.said, stuck.wipes],
          [`npm install failed: timed out after ${INSTALL_TIMEOUT_MS / 1000}s; its half-written node_modules could not be moved aside (EPERM)`, 1],
          "and says so when that tree would not move",
        );

        // An npm that failed on its own exited through its own error handling:
        // nothing is still writing, and what it left is the damage check's to
        // judge (settleInstall). Nothing is moved here.
        const rejected = ran(
          1,
          "npm error code E404\nnpm error 404 Not Found - GET https://registry.npmjs.org/nope\n"
          + "npm error A complete log of this run can be found in: x.log\n",
        );
        const refused = await install(rejected);
        assertDeep(
          [refused.said, refused.said.includes("E404"), refused.wipes],
          [`npm install failed: ${npmFailureReason(rejected)}`, true, 0],
          "an install npm refused is reported by its reason, and the stage is left as it is",
        );
        // A spawn that fails closes with a negative libuv error code as its
        // status (-2 for ENOENT on POSIX, -4058 on Windows); npmFailureReason
        // reads only the error.
        const unspawned = await install(ran(-2, "", Object.assign(new Error("spawn npm ENOENT"), { code: "ENOENT" })));
        assertDeep([unspawned.said, unspawned.wipes], ["npm install failed: spawn npm ENOENT", 0], "as is one whose npm never started");
      },
    },
    {
      name: "an install runs no lifecycle script, and a script it would need is named",
      run() {
        // The arguments. Sealed everywhere; on Windows every path is quoted,
        // since cmd.exe splits the command line on spaces and a user name
        // with one split the stage in two.
        const spaced = join("/", "Users", "A B", "Temp", "oam-mcp-matrix");
        const shellAt = join(spaced, basename(NO_SCRIPT_SHELL));
        const gitAt = join(spaced, basename(NO_GIT));
        const sealed = (q) => [
          "install", "--no-save", "--no-audit", "--no-fund", "--ignore-scripts",
          q(`--script-shell=${shellAt}`), q(`--git=${gitAt}`), "--allow-git=none", "--prefix", q(spaced), "x@latest",
        ];
        assertDeep(npmInstallArgs(["x@latest"], { dir: spaced, platform: "win32" }), sealed((a) => `"${a}"`), "on Windows the install is sealed and every path quoted");
        assertDeep(npmInstallArgs(["x@latest"], { dir: spaced, platform: "linux" }), sealed((a) => a), "elsewhere it is sealed and nothing is quoted");
        assertDeep(
          [dirname(NO_SCRIPT_SHELL) === stage, dirname(NO_GIT) === stage, basename(NO_SCRIPT_SHELL) !== basename(NO_GIT)],
          [true, true, true],
          "the two refusers live in the stage the run holds, under names that tell them apart",
        );
        // And they arrive whole: spawned the way npmInstall spawns (through
        // cmd.exe on Windows), a node that prints its argv gets each path as
        // one argument, space and all.
        const printArgv = "process.stdout.write(JSON.stringify(process.argv.slice(1)))";
        const viaShell = process.platform === "win32";
        const echoed = spawnSync(
          viaShell ? `"${process.execPath}"` : process.execPath,
          ["-e", viaShell ? `"${printArgv}"` : printArgv, ...npmInstallArgs(["x@latest"], { dir: spaced })],
          { shell: viaShell, encoding: "utf8", timeout: 30_000 },
        );
        assertDeep(JSON.parse(echoed.stdout || "null"), sealed((a) => a), "the quoted paths reach the program as single arguments");
        assertDeep(
          [
            stagePathProblem(join("/", "Users", "%USERNAME%", "Temp"), "win32") !== null,
            stagePathProblem('C:\\a"b', "win32") !== null,
            stagePathProblem(spaced, "win32"),
            stagePathProblem(join("/", "home", "100%"), "linux"),
          ],
          [true, true, null, null],
          "a stage cmd.exe would rewrite is refused on Windows, and only there",
        );

        // A file npm would run as a refuser: on Windows the .com and .exe
        // libuv tries, never the bare name; elsewhere the bare name.
        const plantedAt = (platform, path) => plantedRefuser((p) => p === path, platform);
        assertDeep(
          [
            plantedAt("win32", `${NO_GIT}.exe`),
            plantedAt("win32", `${NO_SCRIPT_SHELL}.com`),
            plantedAt("win32", NO_GIT),
            plantedAt("linux", NO_SCRIPT_SHELL),
            plantedAt("linux", `${NO_GIT}.exe`),
            plantedAt("win32", `${NO_GIT}.bat`),
          ],
          [`${NO_GIT}.exe`, `${NO_SCRIPT_SHELL}.com`, NO_GIT, NO_SCRIPT_SHELL, null, null],
          "a planted refuser is found where npm would run it -- on Windows as .com or .exe",
        );

        // Why an install was refused, from npm's own lines (shapes captured
        // from npm 11.13.0 on Windows and 10.9.8 on Linux).
        const npmSaid = (...lines) =>
          `${lines.map((l) => `npm error ${l}`).join("\n")}\nnpm error A complete log of this run can be found in: C:\\npm\\x.log\n`;
        const reason = (stderr) => npmFailureReason({ status: -4058, signal: null, stdout: "", stderr, error: null });
        assertDeep(
          reason(
            npmSaid(
              "code EALLOWGIT",
              'Fetching packages of type "git" have been disabled',
              'Refusing to fetch "npm-life-cycle-scripts-sample@github:kimulaco/npm-life-cycle-scripts-sample#8d0807cac0e1a88100a50ee0ebe2aebdbb5d64ba"',
            ),
          ),
          "its dependency tree has a git dependency, and the matrix runs no git: it installs only what the registry serves",
          "npm 11's refusal of a git dependency reads as the git refusal",
        );
        assertDeep(
          [
            reason(npmSaid("code ENOENT", `syscall spawn ${NO_GIT}`, `path ${NO_GIT}`, "errno -4058")),
            reason(npmSaid("code ENOENT", `syscall spawn ${NO_SCRIPT_SHELL}`, "path C:\\stage\\node_modules\\esc-detached", "errno -4058")),
            reason(npmSaid("code ENOENT", `syscall spawn ${NO_SCRIPT_SHELL}`, "path /stage/node_modules/@scope/x", "errno -2")),
            reason(npmSaid("code ENOENT", `syscall spawn ${NO_SCRIPT_SHELL}`, "path /stage/node_modules/a/node_modules/b", "errno -2")),
            reason(npmSaid("code ENOENT", `syscall spawn ${NO_SCRIPT_SHELL}`, "path /home/u/.npm/_cacache/tmp/git-cloneFCcNIK", "errno -2")),
            reason(npmSaid("code ENOENT", `syscall spawn ${NO_SCRIPT_SHELL}`, "errno -2")),
          ],
          [
            "its dependency tree has a git dependency, and the matrix runs no git: it installs only what the registry serves",
            "esc-detached cannot install without running a lifecycle script, and the matrix runs none",
            "@scope/x cannot install without running a lifecycle script, and the matrix runs none",
            "b cannot install without running a lifecycle script, and the matrix runs none",
            "a git dependency in its tree runs a prepare script to build, and the matrix runs no lifecycle script",
            "a package cannot install without running a lifecycle script, and the matrix runs none",
          ],
          "a refused install says what it wanted -- git, or a script, and whose",
        );
        assertDeep(
          [
            reason(npmSaid("code E404", "404 Not Found - GET https://registry.npmjs.org/nope")),
            reason(npmSaid("code ENOENT", "syscall spawn C:\\Windows\\system32\\cmd.exe", "errno -4058")),
            npmFailureReason({ status: null, signal: null, stdout: "", stderr: "", error: Object.assign(new Error("t"), { code: "ETIMEDOUT" }) }),
          ],
          ["npm error code E404", "npm error code ENOENT", `timed out after ${INSTALL_TIMEOUT_MS / 1000}s`],
          "every other failure reads as it did",
        );

        // Which install scripts a package has: what npm runs on install, and
        // the node-gyp build npm makes up for a binding.gyp.
        const at = join("/", "nm", "p");
        const withGyp = (path) => path === join(at, "binding.gyp");
        const none = () => false;
        const p = (extra) => ({ name: "p", version: "1.0.0", ...extra });
        const found = (manifest, exists) => installScriptsOf(at, manifest, exists).map((s) => `${s.pkg} ${s.event}: ${s.script}`);
        assertDeep(
          [
            found(p({ scripts: { preinstall: "a", install: "b", postinstall: "c", prepare: "d", test: "e" } }), none),
            found(p({}), withGyp),
            found(p({ gypfile: false }), withGyp),
            found(p({ scripts: { install: "make" } }), withGyp),
            found(p({ scripts: { preinstall: "setup" } }), withGyp),
            found(p({ scripts: { prepare: "tsc" } }), none),
            found(p({ scripts: { postinstall: 42, install: "  " } }), none),
            found(p({ scripts: "not-an-object" }), none),
          ],
          [
            ["p@1.0.0 preinstall: a", "p@1.0.0 install: b", "p@1.0.0 postinstall: c"],
            ["p@1.0.0 install: node-gyp rebuild"],
            [],
            ["p@1.0.0 install: make"],
            ["p@1.0.0 preinstall: setup"],
            [],
            [],
            [],
          ],
          "install scripts are the three install events plus npm's implicit node-gyp build -- not prepare, not a blank or non-string one",
        );

        // One walk gives both facts, the nested copy's script, not the
        // hoisted one's, and the same engines engineRanges gives.
        const tree = new Map();
        const nm = join("/", "stage", "node_modules");
        const put = (parts, manifest) => tree.set(join(nm, ...parts), manifest);
        put(["app"], { name: "app", version: "1.0.0", engines: { node: ">=18" }, dependencies: { lib: "^1", tool: "^1" } });
        put(["app", "node_modules", "lib"], { name: "lib", version: "1.0.0", scripts: { postinstall: "node nested.js" } });
        put(["lib"], { name: "lib", version: "2.0.0", scripts: { postinstall: "node hoisted.js" } });
        put(["tool"], { name: "tool", version: "3.0.0", engines: { node: ">=20" }, scripts: { install: "node build.js" } });
        const read = (dir) => tree.get(dir) ?? null;
        const facts = closureFacts(join(nm, "app"), read, none);
        assertDeep(
          [facts.scripts.map((s) => `${s.pkg} ${s.event}: ${s.script}`), facts.engines],
          [
            ["lib@1.0.0 postinstall: node nested.js", "tool@3.0.0 install: node build.js"],
            [
              { pkg: "app@1.0.0", range: ">=18", dependency: false },
              { pkg: "tool@3.0.0", range: ">=20", dependency: true },
            ],
          ],
          "the tree's scripts are the ones node would load, beside its engines floors",
        );
        assertDeep(
          skippedScriptsOf({ scripts: facts.scripts }),
          ["lib@1.0.0 postinstall: node nested.js", "tool@3.0.0 install: node build.js"],
          "the report records every script the row's install skipped",
        );
        assertDeep(skippedScriptsOf({ error: "x" }), [], "and none for a row that never resolved");
        assertDeep(closureFacts(join(nm, "absent"), read, none), { engines: [], scripts: [] }, "a package not on disk has neither");

        // The review: exact on name, event and text.
        const puppeteer = { pkg: "puppeteer@23.11.1", name: "puppeteer", event: "postinstall", script: "node install.mjs" };
        const others = [
          { ...puppeteer, script: "node install.mjs --force" },
          { ...puppeteer, event: "install" },
          { ...puppeteer, pkg: "puppeteerx@1.0.0", name: "puppeteerx" },
        ];
        assertDeep(
          [unreviewedScripts([puppeteer]), unreviewedScripts(others).length],
          [[], 3],
          "puppeteer's reviewed postinstall passes; changed text, another event or another package does not",
        );
        assertDeep(
          describeScripts(others),
          "puppeteer@23.11.1 postinstall `node install.mjs --force`; puppeteer@23.11.1 install `node install.mjs`; 1 more",
          "a row names two scripts and counts the rest",
        );
        for (const r of REVIEWED_INSTALL_SCRIPTS) {
          assertDeep(
            [["preinstall", "install", "postinstall"].includes(r.event), [r.name, r.script, r.why].every((v) => typeof v === "string" && v.trim() !== "")],
            [true, true],
            `the review of ${r.name} names an install event, the script, and why skipping it is faithful`,
          );
        }

        // The row: an install error first, then damage, then an unreviewed
        // script, each a SKIP keeping the version; a reviewed one passes.
        const resolved = (scripts) => () => ({ entry: "/bin/x", version: "1.0.0", engines: [], scripts });
        const unreviewedTool = [{ pkg: "tool@3.0.0", name: "tool", event: "install", script: "node build.js" }];
        const noErrors = new Map();
        const brokenTree = new Map([["x", [{ from: "x@1.0.0", name: "gone", hollow: null }]]]);
        const skipped = binFor("x", noErrors, noErrors, false, resolved(unreviewedTool));
        assertDeep(
          [skipped.version, skipped.error?.includes("tool@3.0.0 install `node build.js`"), skipped.error?.includes("REVIEWED_INSTALL_SCRIPTS")],
          ["1.0.0", true, true],
          "an unreviewed install script makes the row a SKIP that names it, version kept",
        );
        assertDeep(binFor("x", noErrors, noErrors, false, resolved([puppeteer])).entry, "/bin/x", "a reviewed one leaves the row to run");
        assertDeep(binFor("x", noErrors, brokenTree, false, resolved(unreviewedTool)).error?.startsWith("its install is not whole"), true, "damage is reported first");
        assertDeep(binFor("x", new Map([["x", "npm install failed: E404"]]), brokenTree, false, resolved(unreviewedTool)), { error: "npm install failed: E404" }, "and an install error before anything");
      },
    },
    {
      name: "an interrupted run ends its npm, then dies of the interrupt",
      async run() {
        // A stand-in npm that never exits, in flight under runBounded, and an
        // interrupt with the real tree kill and a recorded rename, lock
        // release and death. Every process runBounded starts here is counted
        // and kept, so one started when none may be is seen -- and still
        // cleaned up.
        const children = [];
        const spawnFn = (...spawnArgs) => {
          const child = spawn(...spawnArgs);
          children.push(child);
          return child;
        };
        const standIn = ["-e", "setInterval(function () {}, 1000)"];
        let result = "pending";
        runBounded(process.execPath, standIn, { timeoutMs: 60_000, spawnFn }).then((r) => {
          result = r;
        });
        for (let waited = 0; inFlight.size === 0 && waited < 3_000; waited += 20) await sleep(20);
        const pid = [...inFlight][0]?.pid;
        const steps = [];
        const seams = {
          kill: async (p) => {
            steps.push("kill");
            await killTree(p);
          },
          wipe: async () => {
            steps.push("wipe");
            return null;
          },
          release: () => steps.push("release"),
          die: (signal) => steps.push(`die ${signal}`),
          note: () => steps.push("note"),
        };
        let alive = true;
        let started = 0;
        let steps1 = [];
        let idle = [];
        // `ending` is the whole module's: every later case needs runBounded
        // back, so it is reset however this one ends.
        try {
          // A second interrupt lands while the first is still killing, and
          // does not wait for it: it releases the lock and dies before the
          // first's rename. The first says what it did only once npm is dead
          // -- a terminal gone away can fail that write.
          const firstEnding = endRun("SIGINT", seams);
          await endRun("SIGTERM", seams);
          await firstEnding;
          steps1 = steps.splice(0);
          alive = Number.isInteger(pid) && isAlive(pid);
          for (let waited = 0; alive && waited < 3_000; waited += 100) {
            await sleep(100);
            alive = isAlive(pid);
          }
          await sleep(200); // room for a settle that must not happen
          // Nothing starts once the run is being ended -- not even an install
          // called just before the interrupt was handled. That is the
          // interrupt which arrived during synchronous work: it is handled on
          // the event loop's next turn, which no microtask reaches, so
          // runBounded must not spawn before that turn. The callback here
          // sets `ending` on it, ahead of runBounded's own, as endRun would.
          ending = false;
          const before = children.length;
          setImmediate(() => {
            ending = true;
          });
          runBounded(process.execPath, standIn, { timeoutMs: 60_000, spawnFn });
          await sleep(200);
          started = children.length - before;
          // With no install in flight, an interrupt releases the lock and dies
          // at once, moving nothing: the stage is whole, or the damage check's
          // to judge.
          ending = false;
          await endRun("SIGINT", seams);
          idle = steps.splice(0);
        } finally {
          ending = false;
          for (const child of children) if (child.exitCode === null && child.signalCode === null) await killTree(child.pid);
        }
        assertDeep(
          [Number.isInteger(pid), alive, result, started],
          [true, false, "pending", 0],
          "npm is killed before the matrix dies, and the run goes no further -- no result, nothing new started",
        );
        assertDeep(
          steps1,
          ["kill", "release", "die SIGTERM", "note", "wipe", "release", "die SIGINT"],
          "its half-written tree is moved aside only once it is dead, then the lock is released and the matrix dies -- and a second interrupt does that at once",
        );
        assertDeep(idle, ["release", "die SIGINT"], "an interrupt between installs releases the lock and dies at once, and moves nothing");
        // And the death is an interrupt's, in a process of its own: dieOf must
        // end it the way a Ctrl-C does -- STATUS_CONTROL_C_EXIT on Windows,
        // the signal elsewhere -- never with an exit a calling shell could
        // take as handled. The child holds a SIGINT listener, as the matrix
        // does, which a re-raised signal must not land in; it would otherwise
        // live until dieOf's 2s fallback and exit 130 -- an exit a calling
        // shell could take as handled.
        const died = spawnSync(
          process.execPath,
          ["--input-type=module", "-e", `import { constants as osConstants } from "node:os";\n${dieOf}\nprocess.on("SIGINT", () => {});\ndieOf("SIGINT");\nsetTimeout(() => {}, 10_000);`],
          { encoding: "utf8", timeout: 30_000 },
        );
        assertDeep(
          [died.status, died.signal],
          process.platform === "win32" ? [0xc000013a, null] : [null, "SIGINT"],
          "the matrix dies of the interrupt, so a calling script stops too",
        );
      },
    },
    {
      name: "the engines check gives node-semver's answers, or none",
      run() {
        // Expected values are node-semver 7.7.4's satisfies(), except the null
        // rows: syntax this does not read, where deciding nothing is the point
        // (node-semver itself rejects the last two outright).
        const table = [
          [">=22.19.0", "22.22.2", true],
          [">=22.19.0", "22.18.9", false],
          [">=22", "21.9.9", false],
          [">=18", "18.0.0", true],
          [">22", "22.22.2", false],
          [">22", "23.0.0", true],
          [">22.1", "22.22.2", true],
          ["<22", "21.9.9", true],
          ["<22", "22.0.0", false],
          ["<=22", "22.22.2", true],
          ["<=22.22.1", "22.22.2", false],
          ["22", "22.22.2", true],
          ["22.x", "23.0.0", false],
          ["22.22", "22.22.2", true],
          ["=22.22.2", "22.22.2", true],
          ["v22.22.2", "22.22.1", false],
          ["~22.21", "22.22.2", false],
          ["~22", "22.22.2", true],
          ["^22.23", "22.22.2", false],
          ["^22.19.0", "23.0.0", false],
          ["^20.19.0 || >=22.12.0", "22.22.2", true],
          ["^20.19.0 || >=22.12.0", "21.9.9", false],
          [">= 18", "20.11.0", true],
          [">=18 <22", "22.22.2", false],
          ["20 || 22", "22.22.2", true],
          ["*", "22.22.2", true],
          ["", "22.22.2", true],
          [">*", "22.22.2", false],
          ["20 - 24", "22.22.2", null],
          [">=22.0.0-rc.1", "22.22.2", null],
          ["^0.1.0", "22.22.2", null],
          [">=22 garbage", "21.9.9", null],
          ["lts/*", "22.22.2", null],
        ];
        assertDeep(
          table.filter(([range, v, want]) => satisfiesNodeRange(range, v) !== want).map(([range, v, want]) => `${range} @ ${v}: want ${want}, got ${satisfiesNodeRange(range, v)}`),
          [],
          "every row agrees",
        );
      },
    },
  ];

  console.error("mcp-sidecar-matrix --self-test (offline)\n");
  let failures = 0;
  for (const c of cases) {
    try {
      await c.run();
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

/** Resolve an installed package's bin entry point, version, and -- from one
 *  walk of its installed tree (closureFacts) -- the engines.node ranges of it
 *  and its dependencies, the floors nodeHostRefusal holds the node control to,
 *  and the install scripts in it, which binFor holds against the review.
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
  const { engines, scripts } = closureFacts(dir);
  return { entry, version: manifest.version, engines, scripts };
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
 *  set, and `left` -- the processes it left running (null: not checked).
 *  `drainMs` caps the wait for stderr to close (STDERR_DRAIN_MS); only the
 *  self-test sets it. */
function probe(host, entry, { env, scriptArgs = [], oamFlags = [], cwd, call = null, ctx, drainMs = STDERR_DRAIN_MS }) {
  // `--` is REQUIRED before script args: `oam run` declares script_args with
  // clap's `last = true`, so `oam run entry.js serve` is "unexpected argument".
  // This mirrors oam-spawn.ts exactly (`["run", entry, "--", ...rest]` when
  // rest is non-empty, a bare `["run", entry]` when it is not) -- a harness
  // that always appended `--` would still pass while production differs.
  //
  // The node arm is node's own plain invocation, which is what the broker falls
  // back to and therefore the right reference: `node <entry> [...rest]`.
  const cmd = host === "oam" ? oamBin : process.execPath;
  const argv = probeArgv(host, entry, scriptArgs, oamFlags);
  return new Promise((resolveP) => {
    const child = spawn(cmd, argv, { stdio: ["pipe", "pipe", "pipe"], env, cwd, windowsHide: true });
    // 'exit' can fire before the pipe's last stderr chunk is read, and a
    // sidecar that refuses to start prints why and exits at once -- the
    // refusal nodeHostRefusal reads. Resolving waits for stderr to close,
    // capped, because a grandchild that inherited the pipe (a browser) can
    // hold it open long after the sidecar is gone.
    const stderrClosed = new Promise((r) => child.stderr.once("close", r));

    let out = "";
    let stderr = "";
    let settled = false;
    // Which reply the harness is waiting for, so a hang names the phase it hung
    // in: "boots but never answers a tool call" and "never boots" are different
    // bugs and used to print the same line.
    let awaiting = "tools/list";
    // The same fact as a number, for classifyBoot to compare across arms: 0
    // while booting, 1 + i while awaiting the i-th tools/call.
    let depth = 0;
    let tools = [];
    let timer = null;
    const done = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      // Taken now: a reply that lands during teardown can still advance the
      // exchange, and the verdict must describe where it stood when it failed.
      const reached = depth;
      teardown(child)
        .then((left) => Promise.race([stderrClosed, sleep(drainMs)]).then(() => left))
        .then((left) => resolveP({ ...result, depth: reached, left, stderr }));
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
      depth = 1 + step;
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
          // A row that asserts on the tool list itself, not just one tool in it.
          const listWhy = call.expectTools ? call.expectTools(tools) : null;
          if (listWhy) {
            done({ ok: true, tools, call: { ok: false, why: `tools/list: ${listWhy}` } });
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

/** One arm's argv after its executable. The oam arm is `run [...oamFlags]
 *  <entry> [-- ...scriptArgs]`; the node arm is `<entry> [...scriptArgs]`
 *  whatever the row's oamFlags. Pure, for the self-test. */
function probeArgv(host, entry, scriptArgs = [], oamFlags = []) {
  if (host !== "oam") return [entry, ...scriptArgs];
  const run = ["run", ...oamFlags, entry];
  return scriptArgs.length > 0 ? [...run, "--", ...scriptArgs] : run;
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
if (argv.includes("--self-test")) process.exit(await selfTest());
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
// Before anything under the stage is touched -- the fixture wipe below
// included -- the run owns it (lockStage), and only then are earlier runs'
// moved-aside node_modules swept away.
const lockProblem = await lockStage(join(stage, ".matrix.lock"));
if (lockProblem) {
  console.error(lockProblem);
  process.exit(2);
}
// From here on an interrupt ends npm before the matrix, and releases the lock.
// A terminal that has gone away (SIGHUP) fails every write to it (EIO). With
// no listener, endRun's note would then end the run with exit 1 -- after npm
// is dead, but possibly before the half-written tree is renamed aside, and not
// as the death by the signal a calling script has to see.
process.stderr.on("error", () => {});
for (const signal of INTERRUPTS) process.on(signal, () => endRun(signal));
await sweepStageTrash();
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
const { installErrors, damaged, rebuilt } = await settleInstall(
  // Two rows can run one package with different arguments (playwright,
  // pw-isolated): it is installed once.
  [...new Set(selected.map((s) => s.pkg))],
  {
    note: (line) => {
      clearProgress();
      process.stderr.write(line);
    },
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
  const bin = binFor(s.pkg, installErrors, damaged, rebuilt, resolveBin);
  const ver = bin.version ?? "?";
  const record = (state, detail, { why = detail, note = null, tools = null, tool = null, extra = [] } = {}) => {
    clearProgress();
    process.stderr.write(row(s.name, BANNER[state], ver, detail));
    for (const line of [note ? `note: ${note}` : "", ...extra]) if (line) process.stderr.write(under(line.slice(0, 160)));
    // The install scripts its tree holds and this install skipped, so the
    // record shows where the install differed from npx's.
    results.push({ name: s.name, pkg: s.pkg, version: bin.version ?? null, state, tool, tools, why, note, installScriptsSkipped: skippedScriptsOf(bin) });
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
  const nodePin = nodePinFor(pin);
  const ctxFor = (host) => ({
    host,
    loopback: fixtures.loopback,
    resp: fixtures.resp,
    browser: fixtures.browser,
    profileDir: join(profiles, `${s.name}-${host}`),
  });
  const envFor = (host, call) => armEnv(host, s, call, ctxFor(host), nodePin, process.env);

  // A call the environment cannot support is not attempted: the sidecar still
  // has to boot and serve tools on oam, and the row says exactly what was
  // missing.
  const unmet = s.call?.requires ? await s.call.requires(ctxFor("oam")) : null;
  const call = s.call && !unmet ? s.call : null;
  const scriptArgs = s.args ?? [];
  const oamFlags = s.oamFlags ?? [];
  const cwdFor = (host) => (s.isolateHome ? makeHome(ctxFor(host).profileDir) : undefined);

  progress(`  ${s.name.padEnd(12)} probing on oam...`);
  const oam = await probe("oam", bin.entry, { env: envFor("oam", call), scriptArgs, oamFlags, cwd: cwdFor("oam"), call, ctx: ctxFor("oam") });

  if (!oam.ok) {
    // Ask the control here too. A sidecar that fails at boot, initialize or
    // tools/list is the MOST common upstream break -- a bad publish, a missing
    // peer dep, an engines bump -- and blaming oam for it means a release is
    // blocked by somebody else's broken package, with the log saying oam did
    // it. Only the tools/call arm used to be adjudicated, so exactly the shape
    // most likely to be upstream was the one never checked.
    progress(`  ${s.name.padEnd(12)} node control (boot)...`);
    const control = await probe("node", bin.entry, { env: envFor("node", call), scriptArgs, cwd: cwdFor("node"), call, ctx: ctxFor("node") });
    // A control that cannot run this sidecar on this box's node is no evidence
    // at any depth -- the one shape depth comparison cannot see, since a node
    // refused at boot and an oam that fails at boot both stop at depth 0.
    const refused = nodeHostRefusal(bin.engines, process.versions.node, control.stderr);
    const v = classifyLeaks(classifyBoot(oam, control, refused), oam.left, control.left);
    record(v.state, v.why, { note: v.note, extra: [diagnosis(oam.stderr)] });
    continue;
  }
  if (!call) {
    const { state, why: reason } = uncalledVerdict(s, unmet);
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
  const control = await probe("node", bin.entry, { env: envFor("node", call), scriptArgs, cwd: cwdFor("node"), call, ctx: ctxFor("node") });
  // probeFailed keeps two very different facts apart. "The control ran the tool
  // and it failed" is evidence the sidecar is broken; "the control never got far
  // enough to invoke anything" is no evidence at all, and folding them together
  // let a REAL oam regression be excused as upstream whenever the control host
  // could not boot the sidecar (an engines bump past the release box's node,
  // say). A missing control must never exonerate oam -- and neither must one
  // that ran on a node the sidecar does not support, and failed there.
  const refused = nodeHostRefusal(bin.engines, process.versions.node, control.stderr);
  const nodeVerdict = controlVerdict(control, refused);

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
    `${JSON.stringify({ schema: "oam-mcp-sidecar-matrix/1", oam: oamVersion, node: process.version, platform: `${process.platform}-${process.arch}`, exitCode: code, stage: { rebuilt, damaged: Object.fromEntries(damaged), installErrors: Object.fromEntries(installErrors) }, sidecars: results }, null, 2)}\n`,
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
