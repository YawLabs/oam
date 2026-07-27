# Streams Port: Node v22 internal/streams into oam

Status: design, pre-implementation
Owner: oam runtime
Driver: MCP-server stdio/pipe/backpressure fidelity + hono/fastify request/response streams. Priority order: Readable/Writable state machines, pipeline()/finished() > operators/webstream interop > exotic surfaces.

---

## 1. Goal

Replace the hand-rolled ~2.5k-line stream section of `js/node_compat.js` (lines 7158-9705) with Node v22's actual `lib/internal/streams/*` + `lib/stream.js`, vendored nearly verbatim, driven by a small shim prelude. Erase the documented divergences (per-chunk setEncoding without StringDecoder, no `_writev`/cork batching, non-Node `'readable'` scheduling, `finished()` ignoring its options, no-op `setDefaultHighWaterMark`). The vendored stream suite (91 pass / 73 fail today) is the acceptance harness and regression ratchet, not a percentage gate -- the driver is MCP-server stdio/pipe/backpressure and hono/fastify request/response fidelity (2026-07-25 decision: no node-suite % gate). All existing consumers (`fs`, `child_process`, `zlib`, `crypto`, `http`, `http2`, `tls`, stdio, readline) keep working through the same `require('stream')` seam without API changes.

Non-goals: replacing `js/streams.js` (WHATWG web streams stay as-is; only `toWeb`/`fromWeb` bridge to them), porting `lazy_transform.js` (crypto already extends `stream.Transform` directly), converting `net.Socket` into a real Duplex (out of scope; its fake state POJOs are extended, not replaced).

## 2. Source choice: vendor Node v22 core + shim (NOT readable-stream)

**Decision: vendor `lib/stream.js`, `lib/stream/promises.js`, and 17 `lib/internal/streams/*.js` files from the exact Node tag matching the conformance corpus (`v22.22.2`, per `conformance/vendor/node/manifest.json:3`), plus a hand-written shim layer modeled on readable-stream's `ours/` files.**

Evidence:

- readable-stream 4.7.0 is built from **Node 18.19.0** (`package.json` build script pins the tarball), last commit 2025-01-07, dormant 18+ months. It predates `duplexpair.js`, misses v19-v22 fixes in eos/pipeline/webstream interop and `Symbol.asyncDispose` semantics. Our acceptance gate is the **v22.22.2** vendored test corpus -- shipping v18 behavior guarantees differential failures on the exact tests we're chasing.
- readable-stream's real value is as **proof + recipe**: its entire de-Node-ification is 3 shim files (`ours/primordials.js` 3.3KB, `ours/errors.js` 10.1KB, `ours/util.js` 4KB) plus a mechanical replacements table. That proves the shim surface is ~18KB of plain JS. We port the recipe against v22 sources instead of vendoring its stale output.
- Vendoring core keeps re-vendoring cheap on future Node bumps: bodies stay byte-identical (see layering), so a version bump is re-fetch + re-run the mechanical shim-surface derivation + rerun the suite.

Vendor cut (19 files, ~6.7k lines):

- `internal/streams/`: `utils`, `state`, `legacy`, `from`, `destroy`, `end-of-stream`, `add-abort-signal`, `readable`, `writable`, `duplex`, `transform`, `passthrough`, `duplexify`, `duplexpair`, `pipeline`, `compose`, `operators` (17)
- `lib/stream.js`, `lib/stream/promises.js`
- Deferred: `lazy_transform.js` (only needed if crypto streams are re-plumbed), `stream/consumers.js` (keep oam's existing 30-line for-await version initially), `stream/web.js` (oam's `stream/web` alias already re-exports `js/streams.js` globals), `internal/webstreams/adapters` (shimmed to a throwing stub in slice 1, real bridge to `js/streams.js` in a follow-up -- current `to/fromWeb` behavior is preserved by keeping oam's existing bridge functions attached, see 5.4).

Hard constraint from upstream: `utils.js` / `readable.js` / `writable.js` / `destroy.js` share the kState bitfield layout -- **all vendored files must come from the same tag**. Record the tag + commit SHA in every provenance banner.

## 3. Layering

### 3.1 On-disk layout

```
js/vendor/node-streams/
  UPSTREAM            # tag, commit SHA, retrieval date, file list, re-vendor procedure
  internal/streams/*.js   # byte-identical upstream bodies + provenance banner comment only
  stream.js
  stream/promises.js
js/vendor/oam-shims/
  primordials.js      # 36-name plain-JS primordials object
  errors.js           # 17 ERR_* codes + AbortError + aggregateTwoErrors, v22 message text
  validators.js       # the 5 validators
  util.js             # once, kEmptyObject, SymbolDispose/SymbolAsyncDispose, debuglog noop, types
  loader-prelude.js   # __oamVendor mini-CJS loader (define/require, circular-safe)
  register.js         # replaces globalThis.__oamNode.factories.stream with the port
```

### 3.2 Module wrapping: build.rs, not source edits

`crates/oam_engine/build.rs` currently compiles each listed file as a bare `v8::Script` (build.rs:34-79). Extend it with a ~40-line mechanical wrapper: for each file under `js/vendor/node-streams/` (manifest-ordered), emit and evaluate

```js
globalThis.__oamVendor.define("<node-specifier>", function (require, module, exports, process, primordials) {
<verbatim file body>
});
```

- Vendored bodies stay **byte-identical** to upstream (banner comment aside): `require('internal/streams/utils')` etc. resolve through the mini-loader keyed on Node's own specifier strings -- zero path rewrites, zero regex replacement table.
- `primordials` and `process` arrive as wrapper params (Node's own internal wrapper shape), so nothing pollutes the snapshot's global scope. `process` is resolved lazily at require time (`globalThis.process`), satisfying the snapshot constraint.
- Evaluation order in `js_files`: `bootstrap.js`, `node_compat.js`, **`loader-prelude.js`, shims, vendor files (dependency order: utils -> state/legacy/from -> destroy -> end-of-stream/add-abort-signal -> readable -> writable -> duplex -> transform/passthrough/duplexify/duplexpair -> pipeline -> compose -> operators -> stream.js -> stream/promises.js), `register.js`**, then `streams.js` and the rest as today. `rerun-if-changed` per file as now.

### 3.3 Loader semantics

`__oamVendor.define(id, fn)` stores the factory; `__oamVendor.require(id)`:

1. Cache hit -> return cached `module.exports` (**cache-before-execute**: the module object goes into the cache before the body runs, so Node's designed circular requires -- readable <-> duplex <-> writable, duplexpair -> 'stream' -- get partial exports exactly as Node's CJS does).
2. Shim specifiers (`internal/errors`, `internal/validators`, ...) resolve to the oam-shims modules; public specifiers (`events`, `buffer`, `string_decoder`, `stream`, `stream/promises`, `process` never required) resolve to `globalThis.__oamNode.get(name)` at call time.
3. Lazy `??= require(...)` sites in upstream (`duplex.js:201`, `pipeline.js:89,313`, `end-of-stream.js:58`) are preserved verbatim -- the loader is called at those points at runtime, never hoisted.

### 3.4 Snapshot constraints and cost

- `define()` calls at snapshot eval time only store functions -- no `__oam`/`console`/`process` touched at top level. Module bodies execute at first `require('stream')` at runtime, where natives exist. Same rule the existing factories obey (node_compat.js:11-14).
- ~7.5k added lines grow the snapshot blob (reported via `cargo:warning`) and build-time compile only; zero runtime parse cost (`FunctionCodeHandling::Keep`). No V8 string-length concern at this scale.

### 3.5 Kill switch

`register.js` wraps the factory swap: at first require, if `OAM_LEGACY_STREAMS=1` is set (env read at require time via natives -- allowed), fall through to node_compat's legacy factory (kept intact through slice 4, deleted in slice 5). One release of overlap, then the switch and the legacy section both go.

## 4. Prerequisite: process.nextTick ordering

48 `process.nextTick` call sites in the vendored code (readable 9, writable 10, destroy 9); Node's `'data'`/`'end'`/`'error'`/`'close'` sequencing observably depends on nextTick-queue FIFO draining ahead of promise jobs. oam's `process.nextTick` is currently `queueMicrotask(() => fn(...))` (node_compat.js:6416-6421) -- no separate queue.

**Decision: land a JS trampoline first; defer engine work until measured.**

- **Slice 0 (pre-port):** real FIFO nextTick queue in node_compat.js: first `nextTick` in a burst schedules one `queueMicrotask(drain)`; `drain` runs the queue to exhaustion (including entries appended mid-drain) before returning. Route through the ALS-bound `queueMicrotask` wrapper (18796-18804) so AsyncLocalStorage frames keep propagating. This gives (a) FIFO among nextTicks, (b) a full nextTick batch draining ahead of any microtask queued after the batch started -- which covers the intra-stream orderings (emitReadable/afterWrite/emitCloseNT sequencing) that the stream machinery itself relies on.
- **Known residual divergence:** a promise job enqueued *before* the first nextTick of a burst still runs first (Node runs nextTick ahead of already-queued promise jobs). Fixing that requires Rust-side event-loop integration (explicit microtask policy; host drains the JS nextTick array, then `PerformMicrotaskCheckpoint`, loop until both empty). **Do not build this speculatively.** After slice 4, triage remaining stream-suite failures: if a material cluster blames promise-vs-nextTick interleave, open the engine workstream as its own design; otherwise accept and document.

## 5. Shim spec

Keyed to the upstream inventory; everything is plain JS inside `js/vendor/oam-shims/`, loaded via the same `define()` mechanism.

### 5.1 `primordials` (wrapper param, ~120 lines)

Exactly the verified 36-name union: ArrayIsArray, ArrayPrototypeIndexOf, ArrayPrototypePop, ArrayPrototypePush, ArrayPrototypeSlice, Boolean, Error, FunctionPrototypeCall, FunctionPrototypeSymbolHasInstance, JSONParse, MathFloor, Number, NumberIsInteger, NumberIsNaN, NumberParseInt, ObjectDefineProperties, ObjectDefineProperty, ObjectGetOwnPropertyDescriptor, ObjectKeys, ObjectSetPrototypeOf, Promise, PromisePrototypeThen, PromiseReject, PromiseResolve, PromiseWithResolvers, ReflectApply, ReflectOwnKeys, SafeSet, StringPrototypeToLowerCase, Symbol, SymbolAsyncIterator, SymbolFor, SymbolHasInstance, SymbolIterator, SymbolSpecies, TypedArrayPrototypeSet. Implementation notes: `SafeSet = Set`; `FunctionPrototypeSymbolHasInstance` via `Function.prototype[Symbol.hasInstance].call`; `PromiseWithResolvers` -- probe V8 at snapshot build, polyfill if absent (ES2024; oam's V8 should have it -- verify once). Re-derive this union mechanically against the pinned tag before landing (the extraction script goes in `UPSTREAM`).

### 5.2 `internal/errors` (~250 lines)

Self-contained error classes with `.code` and **v22-exact message text** for the 17 codes: ERR_ILLEGAL_CONSTRUCTOR, ERR_INVALID_ARG_TYPE, ERR_INVALID_ARG_VALUE, ERR_INVALID_RETURN_VALUE, ERR_METHOD_NOT_IMPLEMENTED, ERR_MISSING_ARGS, ERR_MULTIPLE_CALLBACK, ERR_OUT_OF_RANGE, ERR_STREAM_ALREADY_FINISHED, ERR_STREAM_CANNOT_PIPE, ERR_STREAM_DESTROYED, ERR_STREAM_NULL_VALUES, ERR_STREAM_PREMATURE_CLOSE, ERR_STREAM_PUSH_AFTER_EOF, ERR_STREAM_UNSHIFT_AFTER_END_EVENT, ERR_STREAM_WRITE_AFTER_END, ERR_UNKNOWN_ENCODING; plus `AbortError` and `aggregateTwoErrors`. Deliberately **not** coupled to node_compat's lexical `codes` registry (it's unreachable from a separate snapshot file, and coupling would drift both). readable-stream's `ours/errors.js` is the reference shape; messages copied from v22.

### 5.3 `internal/validators`, `internal/util`, misc internals

- validators: validateAbortSignal, validateBoolean, validateFunction, validateInteger, validateObject -- exactly 5, copy v22 semantics (message/code via shim errors).
- util: `once`, `kEmptyObject` (frozen `{}`), `SymbolDispose`/`SymbolAsyncDispose` (V8-provided, fallback `Symbol.for`), `promisify.custom = Symbol.for('nodejs.util.promisify.custom')` (must match node_compat's util.promisify detection -- verify), `debuglog: () => noop`, `types.isArrayBufferView/isUint8Array` (native checks).
- `internal/buffer.FastBuffer` -> `Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength)` wrapper.
- `internal/event_target` -> fresh Symbols for kWeakHandler/kResistStopPropagation.
- `internal/abort_controller` -> globals from bootstrap.js:86-142.
- `internal/events/abort_listener.addAbortListener` -> delegate to events factory's `addAbortListener` (exists at node_compat 2318), wrap in `[SymbolDispose]`-able return.
- `internal/blob` -> `{ Blob: globalThis.Blob, isBlob: v => globalThis.Blob && v instanceof Blob }` (only duplexify's Blob branch; degrade gracefully if Blob absent).
- `internal/assert` -> thin `(v, msg) => { if (!v) throw new Error(msg) }` (duplexpair only).
- `async_hooks.AsyncResource` (lazy, eos only) -> identity-bind shim `{ bind: (fn) => fn }`-shaped class unless/until oam grows real async_hooks.
- `internal/webstreams/adapters` -> slice 1: stub whose functions throw ERR_METHOD_NOT_IMPLEMENTED; the module-level `to/fromWeb` entry points get oam's existing bridge implementations attached in `register.js` instead (they already pass e2e `node_stream_transform_pipeline_finished_and_web_interop`). Follow-up slice replaces the stub with a real adapter over `js/streams.js`.

### 5.4 `register.js` (factory swap + oam extras, ~150 lines)

- `globalThis.__oamNode.factories.stream = () => __oamVendor.require('stream')` (behind the 3.5 kill switch). `stream/promises` -> `__oamVendor.require('stream/promises')`; `_stream_*` aliases re-derive as today (node_compat 9664-9668 keep working since they call `registry.get('stream')`); `stream/web` (9669) and `stream/consumers` (9676) untouched. `require('node:stream')` resolves through the same registry name-normalization as every other builtin -- no extra work.
- Attach oam's current `Readable.toWeb/fromWeb`, `Writable.toWeb/fromWeb`, `Duplex.fromWeb` bridges over the vendored lazy-adapter versions until the real adapter lands.
- **Drop the callableCtor Proxy** (node_compat 9581-9603): Node's real ctors are function-style and natively support all three contracted shapes -- `Writable({...})` factory call (instanceof-guard returns `new Writable(options)`), ES5 `Writable.call(this, opts)` grafting (util.inherits / light-my-request / readable-stream shims), and `class X extends Writable` with correct `new.target`. Verified by e2e (crypto Sign/Verify extend Transform, jws-style legacy `Stream.call(this)`) before the proxy is deleted.
- `require('stream')` remains the legacy `Stream` function with everything attached -- that is literally what vendored `lib/stream.js` exports.

### 5.5 Host facts the vendored code reads

- `process.nextTick` -- slice 0 trampoline (section 4).
- `process.platform` -- `state.js:12` makes default HWM 16KiB on win32, **64KiB elsewhere**. oam today uses 16384 everywhere; on linux/mac this is a behavior change that *matches* the v22 corpus and real-Node differential. Audit e2e assertions on HWM defaults (~e2e 8088-8130) and fix them to the v22 values.
- `process.stdout`/`process.stderr` identity -- readable.js:926-927 pipe never-close special case; works since oam's stdio are real module-level objects.
- `string_decoder` -- vendored readable requires it for setEncoding; resolves to node_compat's already-faithful StringDecoder (9713+), finally wiring it in and erasing the split-multibyte mojibake divergence for free.

## 6. Integration plan

The swap is atomic at the module seam (one factory), so "integration" = verifying each consumer against the stricter state machine and fixing the known-divergent seams, in dependency order:

**Wave A -- fs + stdio (same slice as the flip).**
- `fs.ReadStream/WriteStream` (node_compat 5712-5834): today `autoDestroy:false` + hand-emitted `'close'` via nextTick. Under real streams, `destroy()` emits `'close'` unless `emitClose:false` -- without the option we double-emit (the known failure shape). Fix: pass `emitClose:false` in both ctors OR delete the hand-rolled close and let the port own it (prefer the latter where graceful-fs's `.prototype` expectations allow; decide per-class while running `test_fs_stream_*`).
- stdio (6460-6479): stdin `autoDestroy:false` + decorateTty* -- same emitClose audit. e2e: `process_stdin_is_readable_stream`, `writable_write_after_end_without_error_listener_does_not_crash`.

**Wave B -- child_process + zlib + crypto.**
- child_process constructs bare `new Readable({read(){}})`/`new Writable` (16319-16321) -- pure options-form, should Just Work; e2e `child_process_async_spawn_streams_and_events`, `spawn_stdin_piped`.
- zlib `class extends Transform` + bare `super({})` (12753-12849), crypto Sign/Verify extend `stream.Transform` (11265/11314) -- exercises subclassing without the proxy; e2e zlib roundtrips + 20MB gunzip + brotli.
- Real `_writev`/cork now exists; zlib's single-transform model is unaffected, but re-run `http_streaming_writes_stay_ordered`.

**Wave C -- http family + net probes.**
- `http.IncomingMessage extends Readable` with `autoDestroy:false` (13298) -- same emitClose audit as fs.
- ServerResponse/ClientRequest/OutgoingMessage stay EventEmitter pseudo-writables. Vendored `pipeline`/`eos` probe them via vendored `utils.js` predicates: audit `isWritableNodeStream`/`isNodeStream` against these objects (Node's checks are duck-typed on `.write`/`.on` presence; expected to pass, verify). Upgrade-to-real-Writable is a named follow-up, not this port.
- `net.Socket` fakes (14124-14125): enumerate every `_readableState`/`_writableState` field the vendored `utils.js`/`end-of-stream.js`/`pipeline.js` read (mechanically grep the vendored files), extend the fake POJOs to cover the set (candidates beyond the existing endEmitted/length/finished/errorEmitted/needDrain: `closed`, `errored`, `ended`, `destroyed`, `autoDestroy`, `emitClose`, `constructed`). e2e: tls pipe/destroy tests, http2 server stream API, undici shim.
- http2/tls extend Duplex (17210/17393/17588) -- construct-gate + both-sides destroy semantics come from the port now; run their e2e blocks.

**Wave D -- cleanup.** Delete node_compat stream section 7176-9660 (keep the factory line as the delegating stub or move registration fully into register.js), delete the proxy, delete the kill switch, update the header-comment divergence list (7158-7169) to the new (shorter) truth.

## 7. Acceptance harness

Gate: `cargo run -p xtask -- node-suite` (ci-local.sh step 7/9), stream bucket of the 171 vendored `test-stream-*` files; oracle = exit 0; scorecard at `conformance/node-suite-scorecard.json`.

| checkpoint | stream bucket (pass, of 164 runnable) | global minPass ratchet |
|---|---|---|
| today | 91 | 270 (267 linux) |
| after slice 4 (flip + waves A-C) | floor 125, expect ~135 | raise to actual-minus-2 |
| after slice 5 + failure triage | target 140+ (~85%) | raise again |

Rationale for the floor: the port erases the divergence cluster the current scorecard blames for roughly half the 73 failures; the residual is nextTick-interleave (section 4), webstream-adapter stubs, and genuinely exotic surfaces. Every checkpoint re-runs the FULL suite -- consumer buckets (fs/http/zlib/child_process/net) must not regress below their current counts; the ratchet enforces globally.

e2e must-stay-green (crates/oam_cli/tests/e2e.rs): `node_stream_readable_writable_and_pipe_backpressure` (2357), `node_stream_transform_pipeline_finished_and_web_interop` (2395), `stream_add_abort_signal_and_compose` (7410), `readable_async_helpers` (7782), `writable_write_after_end_without_error_listener_does_not_crash` (4801), stream-misc/hwm (~8088 -- expect edits for v22 HWM defaults + functional setDefaultHighWaterMark), `stream/promises` default-import (6386); consumer side: `fs_streams_roundtrip_large_file_in_chunks` + `test_fs_stream_{pipe,end_option,events}`, zlib gzip/gunzip/brotli, child_process spawn streams, `process_stdin_is_readable_stream`, tls pipe/destroy-while-parked, `http2_server_stream_api`, `undici_shim_request_stream`, `http_streaming_writes_stay_ordered`.

Differential: `conformance/cases/05-node-streams.mjs` (byte-identical stdout vs real Node) -- the sharpest ordering check we have; run per slice.

Full `scripts/ci-local.sh` (all 9 steps, 4-platform matrix via release flow) before any tag.

## 8. Licensing step

Node `lib/` JS is MIT ("Copyright Node.js contributors"); one-way compatible with oam's Apache-2.0 single license (no relicensing, consistent with the standing no-dual-license decision).

1. **Retain verbatim** the Joyent MIT header in the 6 files that carry it (`stream.js`, `readable.js`, `writable.js`, `duplex.js`, `transform.js`, `passthrough.js`). Never strip or rewrap.
2. **Provenance banner** on every vendored file: source URL, tag `v22.22.2` + commit SHA, retrieval date, `local modifications: none (loaded via build-time wrapper)` -- matches the hand-vendored V8 precedent. Since the build.rs wrapper keeps bodies byte-identical, the modifications marker stays honest; if any in-file edit ever becomes necessary, add `// Modifications copyright (c) YawLabs, licensed Apache-2.0; original MIT, see THIRD_PARTY_LICENSES.md` under the retained header.
3. **THIRD_PARTY_LICENSES.md**: new hand-maintained entry "Node.js (portions of lib/stream*, lib/internal/streams/*)" carrying BOTH copyright lines ("Copyright Node.js contributors" and "Copyright Joyent, Inc. and other Node contributors") + full MIT text. These files are invisible to cargo-about (same class as the V8 entry), so the entry must survive the ci-local.sh gate 8/9 drift check -- extend the gate's hand-vendored allowlist accordingly.
4. **NOTICE**: one line, "This product includes software developed by Node.js contributors (https://nodejs.org), MIT" -- not required by MIT, added for consistency with the V8 entry.

## 9. Risks + mitigations

| risk | mitigation |
|---|---|
| nextTick != Node ordering breaks event-sequencing tests | Slice 0 trampoline covers intra-stream FIFO; residual promise-interleave divergence measured post-flip, engine workstream only if a failure cluster blames it (section 4) |
| double-`'close'` from autoDestroy:false consumers (fs, stdin, http.IncomingMessage) | emitClose:false / delete hand-rolled close per consumer in waves A/C; e2e fs+stdin tests are the tripwire |
| vendored utils/eos reject net.Socket fake state POJOs -> pipeline/finished break on sockets | mechanical field-read grep of vendored predicates -> extend fakes before the flip (wave C task, done in wave A audit) |
| dropping callableCtor Proxy breaks ES5 grafting / factory-call / new.target consumers | Node ctors natively support all three shapes; keep proxy deletion in slice 5 only after e2e (crypto extends Transform, legacy Stream.call) is green on the port |
| mixed-tag vendoring silently corrupts kState bitfield | single pinned tag recorded in UPSTREAM + banners; re-vendor is all-files-or-none |
| default HWM change on linux/mac (16KiB -> 64KiB) shifts backpressure timing in e2e/bench | intended (matches v22 corpus + differential); audit e2e HWM assertions; note in bench follow-up if MCP throughput shifts |
| loader circular-require bug produces undefined partial exports | cache-before-execute loader + dedicated unit test that requires `duplexpair` (worst cycle) first |
| snapshot eval order regression (vendor files before loader, or before node_compat registry) | build.rs list is explicit + a smoke assert in register.js (`if (!globalThis.__oamNode) throw`) |
| `PromiseWithResolvers` absent in snapshot V8 | probe at build; one-line polyfill in primordials shim |
| stub webstream adapters regress to/fromWeb e2e | register.js keeps oam's existing bridge fns attached until real adapter slice |

## 10. Estimated scope

Net: ~+7.6k lines vendored+shim, ~-2.4k legacy deleted, ~+40 build.rs, small diffs in fs/stdio/http/net seams and e2e assertions.

| slice | content | size | gate |
|---|---|---|---|
| 0 | nextTick FIFO trampoline in node_compat.js + ordering unit tests | ~40 lines + tests | full suite, no regressions (this alone may move existing buckets) |
| 1 | vendor drop: 19 files + UPSTREAM + provenance banners + THIRD_PARTY_LICENSES/NOTICE/gate-8 updates | ~6.7k lines, zero wiring | ci-local gate 8/9 green; no behavior change |
| 2 | loader-prelude + 4 shim files + build.rs wrapper emission; port loads but NOT registered (dark) | ~900 lines JS + 40 Rust | snapshot builds; a hidden `__oamVendor.require('stream')` smoke e2e passes |
| 3 | register.js flip behind OAM_LEGACY_STREAMS kill switch + wave A (fs/stdio emitClose) + net-fake field extension | ~200 lines + seam edits | stream bucket >=125; fs/stdio e2e green; case 05 |
| 4 | waves B+C verification, e2e assertion updates (HWM etc.), scorecard triage, ratchet raise | mostly test diffs | full node-suite, all consumer buckets >= current; ratchet raised |
| 5 | delete legacy section 7176-9660 + proxy + kill switch; header divergence-list rewrite; failure triage report (incl. nextTick-engine go/no-go) | ~-2.4k lines | stream bucket >=140 target; full ci-local; ratchet raised again |

Follow-ups filed, not in scope: real webstream adapters over js/streams.js; http OutgoingMessage-family as real Writables; net.Socket as real Duplex; Rust nextTick engine integration (conditional on slice 5 triage); vendored `stream/consumers.js` swap.

Known flip-introduced regression (slice 3, documented not hidden): `test-stream-compose-operator.js` moved pass->fail -- the throw-AFTER-source-drain block only (`compose(async function*(s){ for await(...){} throw boom })` + `assert.rejects(toArray())`): the boom escapes as an unhandled microtask rejection instead of rejecting toArray(). Throw-before-first-read, throw-mid-stream, and post-construction abort all pass. Class: the section-4 nextTick-in-promise-context residual; owned by the slice-5 triage (engine microtask-policy go/no-go). The differential case 52 covers throw-mid-stream only -- extend it with throw-after-drain WHEN the fix lands (adding it now would redden the gate).
## 11. Slice 5 completion + failure triage report (2026-07-26)

Slice 5 landed: the hand-rolled wave-1 streams (node_compat.js 7337-9840, 2,504
lines: the `node:stream` factory, its `stream/promises` line, and the
`callableCtor` Proxy) are deleted. `js/vendor/oam-shims/register.js` is now the
SOLE definer of `registry.factories.stream` / `stream/promises`; the
`_stream_*` aliases, `stream/web`, and `stream/consumers` factories in
node_compat.js derive through `registry.get("stream")` unchanged. The
`OAM_LEGACY_STREAMS` kill switch is gone -- the env var is silently ignored,
pinned by e2e `legacy_streams_env_var_is_silently_ignored` (vendored identity
holds under the var, nothing on stderr). The fs ReadStream lazy-open fallback
(legacy-only; vendored Readable guarantees construct-before-read) is likewise
deleted. callableCtor needed no replacement: Node's vendored constructors are
function-style and natively support `new`, ES5 `Super.call(this)` grafting,
and factory calls (crypto-extends-Transform + light-my-request e2e have been
green on the port since slice 3).

### Final stream-bucket triage (159/164, 97%)

| test | class | owner |
|---|---|---|
| test-stream-compose-operator | nextTick-in-promise residual (section 4) | engine microtask-policy workstream |
| test-stream-pipeline (2 blocks) | streaming request uploads + socket-level req.abort() | op-handle rework |
| test-stream-readable-async-iterators | socket-level fetch abort | op-handle rework |
| test-stream-pipeline-process | child_process shell spawn on Windows | child_process tranche |
| test-stream-writable-samecb-singletick | async_hooks createHook TickObject events | async_hooks workstream |

Also deferred from the slice-4 review: client disconnect BEFORE the first
response write surfaces no events (needs a per-request native watch channel;
http-lifecycle/op-handle tranche).

### nextTick engine workstream: GO

Recommendation: open the engine microtask-policy integration as its own design
(host drains the JS nextTick array, `PerformMicrotaskCheckpoint`, loop until
both empty -- the shape sketched in section 4). Evidence for GO rather than
accept-and-document: (1) compose-operator throw-after-drain is a LIVE
user-visible failure -- an async-generator error escapes as an unhandled
rejection instead of rejecting the composed stream's consumer -- and its class
is exactly the promise-context tick residual; (2) that failure mode (error
loss in async-iterator composition) sits on oam's core MCP-host path
(SSE/async-iterator pipelines), where an unhandled rejection is a process-
policy event, not a recoverable error; (3) differential case 52's
throw-after-drain extension is blocked on it, so the gap is currently
untestable in the byte-parity harness. The cluster is small (one suite test),
so the workstream is justified by failure MODE, not failure COUNT -- scope it
as the section-4 sketch, sequenced after the public-flip Immediate items.
