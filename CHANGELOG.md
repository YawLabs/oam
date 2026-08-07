# Changelog

All notable changes to oam are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries are derived from the commit history between release tags. Pure internal
churn — formatting passes, lockfile syncs, benchmark receipt refreshes — is
omitted. oam is pre-1.0: breaking changes can land in a minor release.

## [Unreleased]

### Added

- **`--allow-net` and `--allow-env` grants**, with env access actually enforced,
  and **`--carrier` for cross-target `oam compile`** — the compile step can now
  be handed a carrier binary for a target other than the build host.

### Fixed

- **The benchmark harness timed a binary `cargo` could replace mid-run.** `oam`
  is now staged out of `target/` before timing, so a concurrent build cannot
  swap the file underneath a measurement. This invalidated earlier published
  numbers.

## [0.8.2] - 2026-08-06

Release tooling and benchmark measurement only; no runtime behavior change.

### Fixed

- The release flow parks an in-use `oam.exe` by renaming it instead of killing
  whatever process holds it, and lands the version bump on `main` unattended.
- The Windows RSS parser no longer scrambles every reading; benchmarks were
  republished against 0.8.1 with the working parser.

## [0.8.1] - 2026-08-05

### Added

- **Node's own streams.** `require('node:stream')` is now served by a vendored
  copy of Node v22's `internal/streams` sources running over a shim prelude,
  replacing oam's in-house implementation, which was deleted.
- **Host-driven `process.nextTick`** — a FIFO queue with Node-parity exception
  handling, on an explicit V8 microtask policy with engine-owned tick points.
- **Process lifecycle parity**: `beforeExit`, the exit-code matrix, and a
  canonical uncaught-exception ladder.
- **Node eval flags** (`-e` / `-p`), `util.inspect` walker parity, and WHATWG
  `url` host rules.
- **Active-resource introspection** (`process.getActiveResourcesInfo` and
  friends), process warning flags, and `import()` from CommonJS.
- **`.tsx` / `.jsx` support** via the JSX automatic runtime.
- **`async_hooks`** init-observer registry, and `Console` write-callback
  support.
- Node's `assert` message machinery and the `util.inspect` prerequisites it
  needs; `assert.ok` now quotes the failing expression, as Node does.
- Node-shaped fatal reports, plus the `--permission`, `--env-file`, and
  `--input-type` flags, and unref'd ops.
- `child_process`: `stdio: 'ipc'` in `spawn`.
- `worker_threads`: per-worker stdout/stderr capture, `execArgv`, and the `--`
  separator. Workers inherit process flags, and `NODE_OPTIONS` is honored
  behind a strict allowlist.
- `Buffer`: pooled small allocations, and `postMessage` transfer lists.
- `structuredClone` transfer, POSIX identity APIs, and dotenv parity.
- `SECURITY.md`.

### Fixed

- `http`: `req.destroy()` now resets an in-flight response stream. It
  previously hung.
- `child_process`: `child.pid` is available synchronously from `spawn` and
  `exec`, matching Node.
- `process.env`: case-folding, own-property semantics, and inheritance by child
  processes.
- Windows: Node-shape quoting for `exec`, and bare-script invocation.
- `Buffer`: oversized `toString` throws `ERR_STRING_TOO_LONG` instead of
  crashing the process.
- `string_decoder`: rejects invalid UTF-8 lead bytes and lead-illegal second
  bytes.
- A worker that touched `worker_threads` never exited.
- The engine reports the handler's own exception, and no longer stringifies
  rejections eagerly.
- The installer gives an honest error for the unshipped `linux-arm64` target
  rather than failing obscurely.

### Changed

- Node test-suite conformance moved from 384 to 391 of 402 (97.3%); the pass
  floor was raised to match.

## [0.8.0] - 2026-07-22

Release-pipeline hardening. No runtime behavior changed.

### Added

- The install path authenticates with `GH_TOKEN`, so private-repo installs
  work.
- `NOTICE` and third-party attribution ship with the source tree.

### Changed

- Release preflight creates the release tag at `HEAD`, or re-points it if it
  already exists elsewhere, replacing a manual four-command tag dance. The
  already-published check runs before any tag mutation, so a tag whose assets
  are published is never moved.
- The CI gate fails on `THIRD_PARTY_LICENSES` drift.

### Fixed

- The release script verifies the `SHA256SUMS` manifest against the built
  artifacts before uploading anything.
- The attribution gate could never pass as written.
- CI restores conformance artifacts from `HEAD` and reports honestly when it
  discards a regenerated stamp; gate-regenerated conformance stamps are
  auto-restored.
- Three more end-to-end test waits were de-raced.

## [0.7.0] - 2026-07-11

### Added

- **Application compatibility pass**: Fastify boots, `net` reports Node's
  errno values, `ws` tears down correctly, Ed25519 is supported, `pino` writes
  to stdout as expected, and file descriptors are reserved the way Node
  reserves them.
- **Inbound OS signal delivery** — `process.on('SIGTERM' | 'SIGINT' |
  'SIGHUP')`.
- Operational hardening and the TTY raw-mode stack.
- Per-host node-suite pass floors in `xtask`, and a warn-mode `OAM-TS0000`
  diagnostic in the JSON output.

### Changed

- **CI and releases no longer use GitHub Actions.** All workflows were removed
  and replaced by a script suite: `scripts/ci-local.sh` is the gate,
  `scripts/release-local.sh` cuts releases, and cross-platform legs run on
  remote build hosts.

### Fixed

- Loader: module identity is canonicalized so an entry point and a cyclic
  re-import of it resolve to one key rather than loading twice.
- DNS: the resolver forces EDNS0 and falls back to TCP, matching Node.
- `fs`: write streams flush after each chunk, fixing a finish-before-flush
  race.
- The CLI emits an `OAM-TS0005` diagnostic when a warn-mode check misses the
  exit deadline.
- Build hosts install `tsgo` into a user npm prefix; a conformance high-water-mark
  assertion is no longer tied to a specific Node version; a phantom dev
  dependency was dropped.

## [0.6.1] - 2026-07-02

First release published with binaries and a `SHA256SUMS` manifest. Earlier tags
(`v0.2.0` through `v0.6.0`) exist in the repository but were never published as
releases.

### Added

- **Distribution**: the release pipeline and install scripts (unsigned,
  checksummed), `oam self-update` delegating to the canonical installer, and
  GitHub-authenticated installs.
- **Node test-suite conformance harness**: a vendored corpus with an exit-0
  runner, a manifest and scorecard, per-module reporting, and a skip-ratchet.
- **`node:test`** — a subset of Node's built-in test runner.
- **Node compatibility breadth**: the legacy `url.parse` / `resolve` /
  `resolveObject` API, the legacy `constants` module, `global` as a `globalThis`
  alias, the positional `readline.createInterface(stream)` form, classic
  fd-based `fs` calls (both synchronous and callback), `fs.ReadStream` and
  `fs.WriteStream` as real `Readable` / `Writable` subclasses, `zlib` streaming
  class constructors, and Node v22's stream async-iterator helpers.
- **`util.inspect` parity work**: ANSI colors, null-prototype constructor
  labeling, array subclasses, sparse holes and extra properties,
  `ArrayBuffer` / `SharedArrayBuffer` formatting, `numericSeparator`, and
  per-call options in `formatWithOptions`.
- **Resolver**: realpath-based module identity, CommonJS `.` and `..`,
  `require.resolve`, `file://` imports, and npm `package.json` subpath imports
  (`#name`).
- **Extra-fd stdio on Windows and Unix**, which is what lets oam drive Chromium
  over the CDP pipe — browser drivers work cross-platform.
- **HTTP client**: a native undici-API shim that shadows the npm package, and
  `fetch` honoring an undici dispatcher's `connect.lookup` as a real DNS pin.
- **N-API (beta)**: externals, references, wrap/class, bigint, and buffers — 22
  new symbols.
- **Pre-compilation**: a V8 bytecode code-cache for both CommonJS and ES
  modules, bytecode embedded in compiled binaries, a cache opt-out knob, and
  corruption resilience.
- **Installer**: symlink entries, a cross-process lock, and lifecycle scripts
  for trusted packages.
- **Record/replay (beta)**: `performance.now` is recorded, with an end-to-end
  determinism test.
- An opt-in `io_uring` filesystem fast path on Linux.
- macOS `os` and `process` natives via `sysinfo`.

### Performance

- Hardware SHA-256 on aarch64.
- The `fork` prewarm pool warms lazily, on first `fork()`.
- `io_uring` read chunks grow from 64 KiB to 4 MiB, fixing large-file reads.

[Unreleased]: https://github.com/YawLabs/oam/compare/v0.8.2...HEAD
[0.8.2]: https://github.com/YawLabs/oam/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/YawLabs/oam/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/YawLabs/oam/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/YawLabs/oam/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/YawLabs/oam/releases/tag/v0.6.1
