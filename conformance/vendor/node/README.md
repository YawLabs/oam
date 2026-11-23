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
- Tag: **v26.13.1** (matches the `node --version` differential baseline). The single source
  of truth for that version is `.node-version` at the repo root: `xtask
  node-suite` refuses to run when `manifest.json`'s `nodeVersion` disagrees
  with it, `xtask conformance` refuses a `node` on PATH that is not it, and
  the remote build legs provision exactly it (`scripts/lib/node-pin.sh`). A
  Node bump moves the pin and this corpus together.
- Snapshot date: 2026-11-22 (re-vendored from v22.23.5: in test/parallel 149
  files changed upstream, 18 were removed upstream and 98 are new, and 10 older files of
  the listed modules that the earlier snapshots had missed (test-assert.js,
  test-util.js, ...) were added; 6 of
  test/common changed and common/gc.js is new; deps/cjs-module-lexer dropped
  because no v26 test reads it)
- License: MIT (see `LICENSE`, copied verbatim from the tag). The vendored files
  here are Node test sources, used unmodified.

## Layout

```
package.json            {"type":"commonjs"} -- oam reads bare .js as ESM by
                        default outside node_modules; this forces CJS so the
                        tests' top-of-file require('../common') resolves.
manifest.json           per-test overrides, ratchets, pinned corpus size
test/common/            Node's real, UNMODIFIED test harness helpers
test/parallel/          the vendored tests: every upstream test/parallel file
                        of the modules node_suite.rs MODULES lists (assert,
                        buffer, events, path, process, querystring, stream,
                        string-decoder, timers, url, util)
test/fixtures/          the fixtures those tests read
test/addons/            not-a-binding (read as source, never built)
deps/                   the package.json files test-process-versions.js reads
lib/README.md           oam-own placeholder: a lib/ dir with no .env in it
LICENSE
```

`test/common/` is vendored verbatim and loads on oam unpatched.

## Re-vendoring (when bumping the pinned Node version)

1. Bump the tag above and re-pull the files from
   `https://raw.githubusercontent.com/nodejs/node/<tag>/...`; add the new
   upstream test/parallel files of the listed modules, drop the removed ones,
   and pull any common helper or fixture the new set references. Every file
   here except the oam-own four (README.md, manifest.json, package.json,
   lib/README.md) must be the upstream blob at the tag, byte for byte.
2. Keep `test/common/` UNMODIFIED -- if a new Node version's common needs an API
   oam lacks, implement it in oam rather than patching the vendored file (a
   divergent shim would silently pass tests that should fail).
3. Update `nodeVersion` and `expectedTotal` in manifest.json, then re-run
   `cargo run -p xtask -- node-suite` and review the diff in pass counts.
