# Conformance harnesses

Two suites gate oam's compatibility claims (RELIABILITY.md):

1. **WinterTC ECMA-429 / WPT** (this directory) — the Minimum Common Web API is the
   conformance floor for the M1 surface. The harness will check out the relevant
   web-platform-tests subset into `checkout/` (gitignored) and run it against `oam run`.
   Pass rates are ratchet-only in CI: a PR may not reduce them.
2. **Node's own test suite** (`tests/node-suite/`, arrives with M2's node: wave 1) —
   per-module pass rates feed the public dashboard at oam.sh/compat (Deno's
   node-test-viewer model: publish the honest rising curve, even at 20%).

Status: harness skeleton. First wired subset (console, URL, TextEncoder/Decoder, timers)
lands with the M1 surface it tests.
