# Vendored Node.js test corpus

A pinned snapshot of a subset of the [nodejs/node](https://github.com/nodejs/node)
test suite, run against oam by `cargo run -p xtask -- node-suite`. Unlike the
hand-curated `conformance/cases/` differential suite (which compares oam vs Node
stdout byte-for-byte), these are **Node's own self-asserting tests**: each
`require('../common')` + `assert.*`s its expectations and exits non-zero on
failure, so the oracle is simply **exit code 0 == pass** -- no Node baseline
needed at run time.

## Source / pin

- Upstream: https://github.com/nodejs/node
- Tag: **v22.22.2** (matches the `node --version` differential baseline and
  `conformance/runners/surface.mjs`'s Node-22 module list)
- Snapshot date: 2026-06-24
- License: MIT (see `LICENSE`, copied verbatim from the tag). The vendored files
  here are Node test sources, used unmodified.

## Layout

```
package.json            {"type":"commonjs"} -- oam reads bare .js as ESM by
                        default outside node_modules; this forces CJS so the
                        tests' top-of-file require('../common') resolves.
test/common/            Node's real, UNMODIFIED test harness helpers
  index.js                (the universal dependency of every test)
  tmpdir.js
  fixtures.js
test/parallel/          the vendored tests (slice 1: path module)
LICENSE
```

`test/common/index.js` is vendored verbatim. It loads on oam needing only the
`net.getDefault/setDefaultAutoSelectFamilyAttemptTimeout` APIs, which oam now
implements (js/node_compat.js net factory) -- so no patch to common is required.

## Re-vendoring (when bumping the pinned Node version)

1. Bump the tag above and re-pull the files from
   `https://raw.githubusercontent.com/nodejs/node/<tag>/test/...`.
2. Keep `test/common/` UNMODIFIED -- if a new Node version's common needs an API
   oam lacks, implement it in oam rather than patching the vendored file (a
   divergent shim would silently pass tests that should fail).
3. Re-run `cargo run -p xtask -- node-suite` and review the diff in pass counts.

## Scope (slice 1)

This is the first slice of the Node-suite conformance harness. It currently
vendors only the `path` tests and prints a pass/skip/fail tally. A committed
scorecard, a per-test manifest (runnable / skip+reason), per-module reporting,
broader corpus, and an out-of-band CI workflow land in later slices.
