# Roadmap

Strategy: hybrid adoption — every phase is adoptable inside an existing Node project without
switching production runtimes, until 1.0 makes switching boring. Full plan and rationale live
in the project planning docs; this is the operating summary.

| Phase | Target | Ships | Gate |
|---|---|---|---|
| **P0** (now) | month 0-1 | Workspace, governance docs, 6-target CI, V8 hello-world, snapshot pipeline seed, ECMA-429 harness skeleton | CI green on all 6 tier-1 targets |
| **M1 / v0.1** | month ~6 | `oam run` + `oam check`: demo-critical ECMA-429 subset (console, fetch, URL, timers, encoding, core streams), ESM loader, oxc TS strip/transform + tsconfig paths, tsgo sidecar streaming diagnostics, ODIF v1, <=20ms cold start | the demo Node userland cannot replicate: an agent consuming ODIF over MCP runs check->fix->test in one loop |
| **M2 / v0.2-0.3** | month ~11 | `oam test` (mocking + fake timers day 1, fork-isolated files), node: compat wave 1 incl. AsyncLocalStorage, remaining ECMA-429 (WebCrypto etc.), public conformance dashboard, N-API alpha, agent-context sandboxing, MCP server, V8 Inspector/DevTools attach, REPL | real projects' suites pass; first external users |
| **M3 / v0.4-0.6** | month ~17 | `oam install` (correctness-first; provenance verification; scripts-off with `oam trust`), `oam serve` (opt-in worker isolates, io_uring fast path), first published benchmarks, `oam.fork()` checkpoint pools, record-replay beta, `oam:ai` + SSE helpers, install-time pre-compilation | win the published benchmark axes; Windows install 2-4x Bun |
| **M4 / v0.7-1.0** | month ~26 | node: wave 2, N-API beta (sharp/better-sqlite3/esbuild unmodified), `oam compile` (signed binaries), >85% Node-suite pass, LTS effective, Windows perf-parity audit, docs site + update channel | v1.0 GA |

Cut order under constraint: AI-starter features -> own bundler (bless Rolldown) -> slip
`oam install` past 1.0 -> macOS perf tuning. Never cut: Windows CI, the conformance
dashboard, semver gates.
