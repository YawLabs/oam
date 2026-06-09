# oam

**The reliable TypeScript runtime for the AI era.**

> Fast is table stakes. Reliable is the product.

oam is a JavaScript/TypeScript runtime built in Rust on V8, designed around three ideas:

1. **Types-aware execution.** oam strips and runs TypeScript instantly (oxc), while a warm
   TypeScript 7 (`tsgo`) sidecar streams full type diagnostics concurrently — never blocking
   execution. Node strips without checking; Bun executes types-blind. oam closes the loop.
2. **Reliability with receipts.** Public conformance dashboards (Node's own test suite,
   WinterTC ECMA-429, WPT), soak farms, crash-rate telemetry, a behavior-change log on every
   release, and an LTS commitment. Claims come with reproducible numbers or they don't ship.
3. **Built for the AI era, owned by no model vendor.** Every diagnostic oam emits — parse,
   type, runtime, test, install — is ODIF: structured JSON with stable codes and typed repair
   plans that agents consume directly (and humans see pretty-printed from the same stream).
   Sandboxed-by-default agent execution, an MCP server into the runtime's introspection, and
   checkpoint/fork primitives for parallel eval loops.

## Status

Pre-alpha (M0). The engine boundary runs JavaScript end-to-end; everything else is roadmap.
See [ROADMAP.md](ROADMAP.md).

```
oam run hello.js
```

## Building

Rust stable (see `rust-toolchain.toml`). All six tier-1 targets build from the same tree:
windows-x64, windows-arm64, macos-x64, macos-arm64, linux-x64, linux-arm64.

```
cargo build --workspace
cargo test --workspace
```

## Architecture (short version)

- `crates/oam_engine` — the only crate that touches V8 (rusty_v8). Isolates, snapshots, code cache.
- `crates/oam_core` — event loop, op system, Promise<->Future bridge.
- `crates/oam_diagnostics` — ODIF, the diagnostic envelope everything else speaks.
- `crates/oam_cli` — the `oam` binary.
- `xtask` — repo automation (V8 bump bot, snapshot rebuilds, packaging).

Wider layout (loader, web/node compat, http, test runner, package manager, sandbox, tsgo
sidecar, inspector) lands per the roadmap; crates are created when their workstream starts.

## Governance

- License: [Apache-2.0](LICENSE), forever. Contributions under [DCO](CONTRIBUTING.md); no CLA.
- [GOVERNANCE.md](GOVERNANCE.md) — the oam Covenant, foundation triggers, succession.
- [AI-POLICY.md](AI-POLICY.md) — oam is heavily AI-developed, with identical review gates for
  human and AI changes, enforced changeset limits, and published provenance. The mega-merge
  failure mode is structurally impossible here, by policy and by CI.
- [RELIABILITY.md](RELIABILITY.md) — the scorecard: what we measure, publish, and gate on.

Domain: https://oam.sh (0am.sh is defensively registered and redirects here; the installer and
update channel serve from oam.sh exclusively).
