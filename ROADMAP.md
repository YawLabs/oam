# Roadmap

Strategy: hybrid adoption — every phase is adoptable inside an existing Node project without
switching production runtimes, until 1.0 makes switching boring. Full plan and rationale live
in the project planning docs; this is the operating summary.

| Phase | Target | Ships | Gate |
|---|---|---|---|
| **P0** (now) | month 0-1 | Workspace, governance docs, 6-target CI, V8 hello-world, snapshot pipeline seed, ECMA-429 harness skeleton | CI green on all 6 tier-1 targets |
| **M1 / v0.1** | month ~6 | `oam run` + `oam check`: demo-critical ECMA-429 subset (console, fetch, URL, timers, encoding, core streams), ESM loader, oxc TS strip/transform + tsconfig paths, tsgo sidecar streaming diagnostics, ODIF v1, <=20ms cold start | the demo Node userland cannot replicate: an agent consuming ODIF over MCP runs check->fix->test in one loop |
| **M2 / v0.2-0.3** | month ~11 | `oam test` (mocking + fake timers day 1, fork-isolated files), node: compat wave 1 incl. AsyncLocalStorage, remaining ECMA-429 (WebCrypto etc.), public conformance dashboard, N-API alpha, agent-context sandboxing, MCP server, V8 Inspector/DevTools attach, REPL | real projects' suites pass; first external users |
| **M3 / v0.4-0.6** | month ~17 | `oam install` (correctness-first; provenance verification; scripts-off with `oam trust`), `oam serve` (opt-in worker isolates, io_uring fast path), first published benchmarks, `oam.fork()` checkpoint pools, record-replay beta, `oam:ai` + SSE helpers, install-time pre-compilation, **MCP-server host positioning** (see below) | win the published benchmark axes; Windows install 2-4x Bun |
| **M4 / v0.7-1.0** | month ~26 | node: wave 2, N-API beta (sharp/better-sqlite3/esbuild unmodified), `oam compile` (signed binaries), >85% Node-suite pass, LTS effective, Windows perf-parity audit, docs site + update channel | v1.0 GA |

### MCP: two roles, both ours

oam touches the Model Context Protocol (JSON-RPC 2.0 over stdio) in two distinct ways. They are not the same bet and they share no code path.

1. **oam *serves* MCP** — `crates/oam_mcp/` (M1 slice 6, hardened in M2). Coding agents (Claude Code, Cursor) launch `oam` as a subprocess and call `oam_check` / `oam_run` / `oam_project_info` / `oam_explain` over stdio; every tool result is ODIF so the agent loop is `check -> fix -> run` without scraping prose. Streamable HTTP transport (MCP 2026-07-28) lands with `oam_http`. This is shipped; the gate is conformance against MCP spec revisions and additional tools (e.g. `oam_test` once it stabilizes).

2. **oam *hosts* MCP servers** — the open positioning bet. MCP clients spawn server subprocesses (`npx @some/mcp-server`, `tsx server.ts`, etc.); the relevant oam capabilities for being a good target are cold start (M1 gate: <=20ms), the node: surface MCP servers depend on (M2 wave 1: `fs`, `path`, `process`, `Buffer`, `events`, `util`, `assert`, `os`, `tty`, `module`, `async_hooks`, `node:crypto`, `node:stream`, plus JSON modules with import attributes), and pre-compiled install (M3). The pieces exist; nobody has written down that the intersection is a positioning move. M3 adds an "MCP-server host" column to the published benchmark matrix (cold start, idle RSS, first-call latency against `@modelcontextprotocol/sdk` examples) so the bet is measured, not asserted.

### TypeScript surface — what's covered, what's queued

The TS wedge is a positioning bet, not a feature checklist; the *quality* of the strip + the streaming diagnostics is the product. Current state (oxc transformer in `crates/oam_loader/src/lib.rs:21-32, 62-95`):

- **Covered in M1, exercised by e2e:** type annotation strip, non-erasable syntax lowered (enums, namespaces, parameter properties — strictly more than Node's strip-only support; e2e at `crates/oam_cli/tests/e2e.rs:133` covers enum lowering), `import type` elision, namespace emit, `as const`, `satisfies`, `readonly` modifiers, `?.` / `??`.
- **Gated on M2 (npm resolution milestone):** `.tsx` / `.jsx` support. The blocker is the JSX automatic runtime: oxc lowers JSX to `jsx()` / `jsxs()` calls that need the `react/jsx-runtime` import resolved through the module loader, which only becomes usable once npm resolution lands. The diagnostic (`OAM-PARSE0003`) is wired and tested (`e2e.rs:3056-3061`) so the gate is honest, not silent. Until M2 ships, `.tsx` is a clear error, not a crash.
- **Shipped in M1:** `.cts` runs through the same CJS interop path as `.cjs` (oxc `transpile_typescript` on `SourceType::from_path("foo.cts")`, which is both `is_typescript()` and `is_commonjs()`). The previous "ESM TS only" gate was removed because the strip already worked; the OAM-MOD0003 explanation was rewritten to match. The remaining TS surface on this gate is just `.tsx` / `.jsx`.
- **Gated on M2 (same milestone, separate work):** tsconfig `paths` resolution is partially landed in M1; full M2 conformance follows npm resolution.
- **Gated on M3:** `oam compile` (signed binaries) and pre-compiled install; both reduce cold-start cost of TS-heavy MCP servers hosted via oam, which is the M3 positioning.

The TS-optimization expansion is "raise the floor on what runs correctly" (M1/M2 work above) and "raise the ceiling on what runs fast" (M3 install path), not "invent a new type system at runtime."

Cut order under constraint: AI-starter features -> own bundler (bless Rolldown) -> slip
`oam install` past 1.0 -> macOS perf tuning. Never cut: Windows CI, the conformance
dashboard, semver gates.
