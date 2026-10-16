# Design: application startup snapshots (`oam snapshot`, `oam run --snapshot`)

Status: DRAFT FOR REVIEW 2026-10-15. No code. This is Gap 4c of
`docs/design/claude-code-cli-compat.md`, which says it "needs its own design review
first". Nothing here should be implemented before that review, and nothing beyond
PR 0 (section 15) should start before measurement shows that the time is spent where
a snapshot can save it. 4a (section 2) has measured `--version` and `--help`; the
TUI path, which is the go/no-go input, is still unmeasured.

How claims are marked. **(verified)** means checked against the tree at `aca6d2d` or
against the rusty_v8 150.0.0 sources in the cargo registry while writing this doc.
**(inferred)** means reasoned from code or from V8/deno behavior that was not run.
**(node: recalled)** means node's behavior as remembered. Each of those must be pinned
against node v22.22.2 (`lib/internal/v8/startup_snapshot.js`,
`src/node_snapshotable.cc`, `doc/api/v8.md`, `doc/api/cli.md`) before the spec PR.

## 1. Goal and non-goals

**Goal.** Let an application opt in to building a V8 heap snapshot after its own
initialization has run. A later start deserializes that heap and skips both compiling
and top-level evaluation. This is the one startup technique that saves more than oam's
existing bytecode cache: the cache skips compile, but not evaluation.

The model, the JS API and the failure codes follow node's user-land snapshot
(`--build-snapshot` / `--snapshot-blob` / `v8.startupSnapshot`). oam's own command
names sit on top, and oam's rules are stricter where node's are loose (secrets, open
handles, mismatched blobs).

**Non-goals.**
- **Speeding up unmodified apps.** A snapshot saves the work an app does *before* it
  starts doing its real job. An app whose top level parses argv, opens a TTY and
  starts its UI (a bundled CLI like cli.js 2.1.112 is one module whose top level is
  the whole program) has nothing it can stop at. It needs to be split:
  initialization at top level, the real start in
  `v8.startupSnapshot.setDeserializeMainFunction` (or behind `isBuildingSnapshot()`).
  This holds for node too. It caps what 4c can do for the frozen cli.js: probably
  nothing, unless the bundle is patched (inferred; cli.js's top-level structure was
  not inspected for this). See section 2.
- **Loading a snapshot implicitly.** No snapshot is ever picked up from a cache dir,
  from an env var or because "it was there". See section 11.
- **Snapshotting live I/O.** Open sockets, timers, watchers, children, workers and
  native addons are refused, never "best-effort" dropped (section 8).
- **Workers, the fork pool and `oam serve` pool isolates.** They stay on the built-in
  snapshot (section 13).
- **Cross-version or cross-arch blobs.** A blob runs only on the exact oam build that
  made it.
- **Changing oam's own build-time snapshot** (`crates/oam_engine/build.rs`) beyond
  what extending it needs.

## 2. Motivation: what 4a measured

The handoff doc reports the Claude Code TUI prompt at 3.0-3.6 s on oam against 1.4 s
for the native build. Round 1 found that the doc's "parses and compiles 13.7 MB every
start" premise is false for oam. oam's persistent bytecode cache is on by default for
ESM and CJS (`crates/oam_engine/src/code_cache.rs`), so the measured 3.0-3.6 s was
almost certainly a warm-cache number. The headroom a snapshot adds is therefore
**top-level evaluation** (plus module instantiation and linking), not compilation.

Numbers from `claude-code-cli-compat.md` "4a. Measured (2026-10-15)": cli.js 2.1.112
`--version`, oam 0.19.0 / node v22.22.2 on Windows 11 arm64, 9 interleaved rounds on a
heavily loaded host (medians, min in brackets; ms). Pre-eval and eval come from that
section's T1/T2 probe split, a separate 9-round series, so they do not add up to the
wall column. See it for `--help`, bun and the method.

| Run | Wall to exit | Pre-eval / eval | Notes |
|---|---:|---|---|
| node, cold | 1142 (690) | 1006 / 14 | |
| node, `NODE_COMPILE_CACHE` warm | 683 (388) | 675 / 17 | |
| oam, `OAM_CODE_CACHE=0` | 578 (449) | 521 / 13 | parse + compile + evaluate |
| oam, fresh `OAM_CACHE_DIR` | 946 (554) | 574 / 98 | cold plus produce; +85 ms eval is writing the 4.2 MB blob |
| oam, warm `OAM_CACHE_DIR` | 230 (185) | 320 (min 179) / 13 | evaluate only, so this is 4c's ceiling |
| oam, CDP profile split (`--inspect-brk`) | -- | -- | not usable for cache effects: under an attached inspector the warm cache showed no benefit; the T1/T2 split replaces it |
| oam, TUI prompt (eval vs waiting on MCP / hooks / git) | TBD | TBD | needs a ConPTY driver; this is the go/no-go input |
| oam, from snapshot (PR 3 prototype, trivial app) | TBD | TBD | deserialize cost per MB of blob |

What it means for 4c: on `--version` the warm pre-eval is ~180-320 ms, of which
~40-50 ms is oam's own boot, and evaluation is ~13 ms. On `--help`, evaluation is
~310 ms warm, but that is the command's own work (building and rendering commander's
tree), not initialization a snapshot would capture. So for cli.js the most a snapshot
could save on these two paths is part of the warm pre-eval; the TUI path is open.

**Go/no-go rule for PR 1 onward:** proceed only if warm-cache top-level evaluation of
a realistic target app is a large share of its start (proposal: at least 300 ms, or
at least 25% of time to first output) *and* a prototype deserialize of a same-sized
heap costs clearly less. If most of the 3 s is waiting on MCP servers, hooks, git or
network, stop and record that here. A snapshot cannot help with that.

## 3. Node's model (what oam matches)

(node: recalled.)

- **Build:** `node --snapshot-blob snap.blob --build-snapshot entry.js [args]` runs
  `entry.js` and then its event loop until it is empty. It runs the serialize
  callbacks, then writes the heap to `snap.blob`. Node 22 also takes
  `--build-snapshot-config <json>` (`builder`, `withoutCodeCache`).
- **Run:** `node --snapshot-blob snap.blob [args]` deserializes, runs the deserialize
  callbacks, then runs the deserialize main function. Pin down exactly how
  `process.argv` is shaped in that case with a differential test, not from memory.
- **Entry restrictions:** a single-file script. `require()` inside it reaches
  builtins only, so user-land modules are not supported and apps are expected to
  bundle first. An ESM entry is not supported.
- **`v8.startupSnapshot`:**
  - `isBuildingSnapshot()`.
  - `addSerializeCallback(cb[, data])`: runs before serialization, to release
    resources.
  - `addDeserializeCallback(cb[, data])`: runs after deserialization, to rebuild
    resources.
  - `setDeserializeMainFunction(cb[, data])`: the program after deserialization.
  - The `add*`/`set*` methods throw `ERR_NOT_BUILDING_SNAPSHOT` when not building.
  - A second main function throws `ERR_DUPLICATE_STARTUP_SNAPSHOT_MAIN_FUNCTION`.
- **Runtime data:** node refreshes `process.argv`, `process.env`, `execPath`, `pid`
  and similar on deserialize, so their *objects* are live. Values the app *copied* at
  build time stay frozen.
- **Failure:** a snapshot build that cannot proceed exits with code 14 ("Snapshot
  Failure").

**Where oam matches and where it diverges** (each divergence goes into
`docs/node-divergences.md` when it ships):

| Aspect | oam |
|---|---|
| `v8.startupSnapshot` API, error codes, exit 14 | match |
| `--build-snapshot` / `--snapshot-blob` on oam's node-style front end | match (section 10) |
| Single-file CJS entry, builtins-only require | match in phase 1 |
| ESM entry and user-land module graph | **extension** in phase 2: oam is ESM-first, and the motivating app is ESM |
| Open handles at build | **stricter:** refused with a named diagnostic instead of whatever node does with them (inferred: node's checks are partial) |
| Environment during build | **stricter, open question:** scrubbed by default (section 9) |
| Mismatched blob | coded error before V8 sees the bytes (section 7); node's own check is the version string (node: recalled) |

## 4. How oam's built-in snapshot works today (verified)

- `crates/oam_engine/build.rs:570` calls `v8::Isolate::snapshot_creator(None, None)`,
  so there is **no external-reference table**.
- The *default* context is deliberately left pristine (`:573-581`): every
  `Context::New` in a snapshotted isolate deserializes it, which is how `node:vm`
  contexts get intrinsics and nothing else.
- A second context gets every builtin JS file (bootstrap, node_compat, vendored
  streams, test runner, permissions) evaluated into it (`:583-610`). It is added at
  `RUNTIME_CONTEXT_INDEX = 0` (`:611-615`, mirrored at
  `crates/oam_engine/src/lib.rs:63`) and serialized with
  `FunctionCodeHandling::Keep` (`:617-619`).
- The blob is embedded at `lib.rs:68`, handed to V8 at `lib.rs:597`
  (`CreateParams::snapshot_blob(StartupData::from(OAM_SNAPSHOT))`) and
  materialized with `Context::from_snapshot(scope, RUNTIME_CONTEXT_INDEX, ..)` at
  `lib.rs:677-679`.
- **Natives are installed after restore** (`lib.rs:687-694`): `timers::install`,
  `ops::install`, `node_ops::install`, `cjs::install`, then `install_runtime_globals`
  (`lib.rs:1114-1145`), which calls `__oamNode.installRuntimeGlobals()`
  (`js/node_compat.js:42132`) to build `process`, `performance` and the console from
  live natives. The build.rs module doc (`:1-10`) states the reason: keep
  external-reference bookkeeping out of the blob entirely.
- Rust-side state lives in **isolate slots**, set at `lib.rs:630-669`: `TimerQueue`,
  `PendingOps`, `CryptoState`, `napi::AddonRegistry`, `ReplayState`, `ForkPool`
  (top-level runtime only), `Arc<Permissions>` and `WorkerEnv`. `reset_run_slots`
  (`crates/oam_engine/src/modules.rs:736-760`) adds `ActiveHost`, `ModuleMap`,
  `RejectionLedger`, `UncaughtLedger`, `CjsCache` and a fresh `CoreRuntime` (tokio)
  per run. Further slots are `ProcessArgv` (`lib.rs:727`), `InspectorSlot` (`:719`),
  `EntryEvaluation` (`modules.rs:152`), `PendingCycleImports` / `PendingTLAImports`
  (`modules.rs:2980, 2995`), vm-module state (`vm_module.rs:170`) and `PlatformTasks`
  (`platform.rs:328`).

So "the machinery exists" (handoff doc) is half true. The creator/loader plumbing
exists, but it was designed around *never* serializing a Rust-backed function, and
that is exactly what an app snapshot must do.

## 5. The external-reference table

### 5.1 The problem

An app snapshot is taken *after* natives are installed. Its heap holds JS `Function`
objects whose behavior is a Rust callback address: every `__oam.*` binding, every
CJS `require`, the timer globals. V8 serializes such a function as an index into the
embedder's `ExternalReference` table, and it must get the identical table, in the
identical order, at load. The failure modes are:

- A callback missing at **build**: V8 aborts in the serializer (inferred: V8's
  external-reference encoder treats an unknown address as fatal).
- A table that differs at **load**: an index resolves to a *different* function. That
  means silent misbehavior or a crash, with no diagnostic. This is the one failure
  mode that must be made structurally impossible.

Sites that create Rust-backed functions today (verified by grep over
`crates/oam_engine/src`):

| File | Sites | Kind |
|---|---|---|
| `node_ops.rs` | `bind!` macro at `:128-136`, one invocation from `:137` with **329** entries | `Function::new(scope, op)` |
| `ops.rs` | 27 | `Function::new` |
| `timers.rs` | 5 (`:315-330`) | `Function::new` |
| `cjs.rs` | `:71` plus `make_require` `:826-834` (3) | `Function::new`, `Function::builder(..).data(String)` |
| `napi.rs` | `:1278` trampoline, `v8::External` data `:1277` | refused, see 8.2 |
| `vm_context.rs` | `:504` `ObjectTemplate` with interceptor callbacks | refused in v1, see 8.2 |

### 5.2 Generate it from the install lists: one list, two uses

rusty_v8 150 exposes everything needed, and none of it needs `unsafe` (verified):

- `ExternalReference` is a `#[derive(Clone, Copy)] pub union` whose variants include
  `function: FunctionCallback` (`v8-150.0.0/src/external_references.rs:20-37`).
  Constructing a union value is safe Rust.
- `MapFnTo` is public (`lib.rs:217`), and `f.map_fn_to()` yields the
  `FunctionCallback` trampoline for a zero-sized fn item (`support.rs:524-535`).
- `Function::new_raw(scope, FunctionCallback)` and
  `Function::builder_raw(FunctionCallback)` are safe (`function.rs:1009-1031`).
  That removes the reason `bind!` exists ("MapFnTo rejects fn POINTERS, so no
  table-driven loop", `node_ops.rs:126-127`).

Proposal: each native module declares its bindings once, through a macro that expands
to:

1. `fn natives() -> &'static [(&'static str, FunctionCallback)]`, built once in a
   `OnceLock` from `op.map_fn_to()`.
2. An `install` that loops that list with `Function::new_raw`.

A crate-level `external_references()` concatenates the module lists **in a fixed,
documented order**: timers, ops, node_ops, cjs. The table is a
`Cow::Owned(Vec<ExternalReference>)`; rusty_v8 null-terminates it itself
(`isolate_create_params.rs:77-104`). Since install and table read the same list, a
binding cannot be installed without being in the table.

**Completeness gate.** An xtask lint, run in `ci-local.sh`, fails on any
`v8::Function::new(`, `Function::builder(`, `FunctionTemplate::new(` or
`ObjectTemplate` interceptor in `oam_engine/src` outside the registry macro. Two
documented exceptions are napi and vm_context (refused, 8.2). A runtime test builds a
snapshot whose entry walks every `__oam` binding object, deserializes it, and calls a
harmless native from each list.

### 5.3 Hash it

The table's *identity* is a SHA-256 over the ordered binding **names** (with the
module boundaries), not over addresses, which ASLR changes per process. build.rs
already reads every source file. It emits the hash as `OAM_EXTREF_HASH` in the same
way it emits `OAM_DEP_VERSIONS` (`build.rs:234`). A reorder, add or remove changes the
hash, and the blob header carries it (section 7).

### 5.4 Loader cost

Workers and plain `oam run` never pass the table, so their startup is unchanged. Only
an isolate created from an app blob passes it. Building the ~365-entry `Vec` is
microseconds (inferred). The sites in 5.1 add up to about 365 (329 + 27 + 5 + 4).

## 6. Building on the existing snapshot

Use `Isolate::snapshot_creator_from_existing_snapshot(StartupData::from(OAM_SNAPSHOT),
Some(external_references()), Some(params))` (verified: `isolate.rs:963-973`). The
creator then:

1. Materializes the pristine default context with `Context::new` and calls
   `set_default_context(pristine)` (`isolate.rs:1978`), so `vm.createContext` in an
   app-snapshot process still gets intrinsics only. The order matters (inferred from
   deno's use of the same API): a context created from an existing snapshot is
   re-serialized only if it is re-added. Nothing carries over automatically.
2. Materializes the runtime context with
   `Context::from_snapshot(RUNTIME_CONTEXT_INDEX)` and runs the normal install path
   (natives, `installRuntimeGlobals`, the CLI's flag globals).
3. Runs the app build (section 8), then `add_context(runtime)` and asserts it lands
   at `RUNTIME_CONTEXT_INDEX` (`isolate.rs:1996`), exactly as build.rs asserts today.
4. Calls `add_context_data` for the module records (6.1), then
   `create_blob(FunctionCodeHandling::Keep)` (`isolate.rs:2385`). `Clear` sits
   behind `--snapshot-no-code`, matching node's `withoutCodeCache`.
5. Asserts `StartupData::can_be_rehashed()` (`snapshot.rs:69`). If V8 cannot reseed
   hash tables on load, every process from the blob shares the build's hash seed, a
   HashDoS predictability regression. Refuse with OAM-SNAP0013 instead of writing
   such a blob (inferred semantics; verify with a V8 source read).

The isolate setup in `JsRuntime::new_inner` (`lib.rs:579-700`) covers heap caps,
microtask policy, OOM handlers, host callbacks and slots. It is factored into one
`configure_isolate(&mut OwnedIsolate, ..)` that the normal path, the creator path and
the loader path all call, so the three cannot drift.

`create_blob` consumes the isolate and drops the slot annex first (`isolate.rs:2389-
2401`, `dispose_annex`), so Globals held in slots are released before V8 serializes.
The inspector must be detached before that (refuse `--inspect` with `oam snapshot` in
v1).

### 6.1 Module map and module records

Rust holds JS identity in slots as `v8::Global`s, and slots are not part of the heap:

- `ModuleMap` (`modules.rs:62-77`): `by_path: HashMap<PathBuf, Global<Module>>`,
  `paths_by_hash`, `edges`, `pending_code_cache`.
- `CjsCache` (`cjs.rs:35-48`): `modules: HashMap<PathBuf, Global<Object>>`,
  `main_key`, `main` (`require.main`).

At build, the Globals go into one JS array per map, attached with
`add_context_data(runtime, array)`. The Rust-only parts (paths, edges, keys, array
indices) go into the blob header as data. At load,
`get_context_data_from_snapshot_once` (`scope.rs:592`) returns each array once, and
the maps are rebuilt.

- `paths_by_hash` is **recomputed** from the deserialized modules' identity hashes.
  It is never trusted from the build (inferred: identity hashes may not survive
  deserialization).
- `pending_code_cache` must be drained (`store_pending_code_cache`) before build.
- The loader path must **not** call `reset_run_slots` as written
  (`modules.rs:742-747` replaces `ModuleMap` and `CjsCache` with empty ones). A
  `reset_run_slots_from_snapshot` resets only the run-scoped slots.

Phase 1 (CJS single file, node parity) needs only `CjsCache.main`. Phase 2 (ESM
graph) needs all of `ModuleMap`, and dynamic `import()` after deserialize must
resolve new modules against it. Every evaluated V8 module must be in `Evaluated`
state at build, and the entry's evaluation promise (`EntryEvaluation`) must be
settled. A pending top-level await is refused (OAM-SNAP0011). Deno snapshots
evaluated ES modules with the same V8 APIs, so V8 does serialize module records
(inferred, from deno_core's design, not run here).

Paths are absolute and frozen. A blob built in `/a/app` resolves lazy imports and
`require.resolve` against `/a/app`. v1 records the build root and refuses to start if
the entry path no longer exists (OAM-SNAP0002). Relocatable blobs are an open
question.

## 7. Blob format and mismatch errors

```
offset 0   magic        "OAMSNAP\0"            (8 bytes)
           format       u32 LE, starts at 1
           header_len   u32 LE
           header       UTF-8 JSON, header_len bytes
           payload      V8 StartupData bytes (header.payload_len)
```

Header fields:

- `oam_version`, `target` (arch-os-env) and `v8_version`.
- `base_snapshot_sha256`: SHA-256 of `OAM_SNAPSHOT`, which changes whenever any
  builtin JS changes. The app heap *contains* oam's builtin JS, so an upgraded oam
  must not run an old blob's copy of it against new natives.
- `extref_hash` (5.3).
- `v8_flags`: the canonical, sorted set from `init_platform_with_flags`
  (`crates/oam_cli/src/main.rs:651-662`).
- `node_flags`: the CLI flag globals. They are installed as
  `configurable: false` (`main.rs:2754-2760`), so they are baked into the heap and
  cannot be re-applied.
- `code`: Keep or Clear.
- `entry_path`, `entry_kind` and `build_root`.
- `module_map` / `cjs_cache` tables (6.1).
- `has_main_function`, `env_mode` (9) and `payload_len`, `payload_sha256`.

Checks at load, all **before** V8 sees a byte, in this order, each a coded error
(`OAM-SNAP` is a new family next to `OAM-RT`/`OAM-MOD`; the numbers are proposals):

| Code | Meaning |
|---|---|
| OAM-SNAP0001 | not an oam snapshot: bad magic, truncated, header unparsable, unknown `format` |
| OAM-SNAP0002 | built by a different oam: version, target, V8, base snapshot or extref hash differs, or `build_root`/entry is gone. The message names both values ("built by oam 0.20.1 (V8 15.0.x), this is oam 0.21.0: rebuild with `oam snapshot`"). |
| OAM-SNAP0003 | payload corrupt: length or SHA-256 mismatch, or `StartupData::is_valid()` false (`snapshot.rs:82`) |
| OAM-SNAP0004 | V8 flags or node flags differ from the build's |
| OAM-SNAP0010 | build: not quiescent (section 8); lists every offender |
| OAM-SNAP0011 | build: unsupported state (addon loaded, vm context, live worker, pending TLA, an unsettled entry) |
| OAM-SNAP0012 | build: a serialize callback threw (original error attached) |
| OAM-SNAP0013 | build: V8 refused (`create_blob` returned None, or not rehashable) |

Build failures exit with code **14**, as node does (node: recalled). Load failures
exit like any startup error (inferred: node exits 1 there; pin it).

The payload hash costs a full read plus SHA-256 of the blob each start (with `ring`,
probably tens of ms for a 50 MB blob; not measured). It guards against corruption,
not tampering (section 11). Whether to always verify, or check length plus
`is_valid` and hash only under `--snapshot-verify`, is an open question to decide
from the PR 3 measurement.

## 8. Quiescence: what must be empty at build, and how it is checked

Build sequence:

1. Evaluate the entry.
2. Run the event loop (`pump_event_loop`, `modules.rs:1446`) until it would exit
   naturally.
3. Run the serialize callbacks in registration order.
4. Run one more tick-and-microtask drain.
5. **Check.**
6. Serialize.

The check enumerates every unref'd item as well. "Would not keep the loop alive" is
the wrong test: an unref'd timer is still a Rust-side entry that cannot be
serialized.

### 8.1 Named diagnostics

Each check reports what it found and, where oam knows it, *where*. Every offender is
listed in one OAM-SNAP0010 message, not just the first:

| Check | Source of truth | Diagnostic names |
|---|---|---|
| Timers and immediates | `TimerQueue` slot, plus JS `registry._activeTimers` (`node_compat.js:3147`) | `Timeout` / `Immediate`, callback name, delay, refed or not |
| Sockets, servers | JS `registry._activeHandles` (`node_compat.js:3150`, type tags like `TCPSocketWrap`, unref'd included) | type tag, local/remote address |
| In-flight requests | JS `registry._activeRequests` (fs callback layer) and `PendingOps` (`ops.rs:16`) | op name. `PendingOps` maps `OpId -> resolver` only, so add the op name for diagnostics. |
| fs watchers | timers today (`fs.watch` is a `setInterval` poll); `_activeHandles` as `FSEvent` after Gap 3 (PR #409) lands native watchers (inferred: confirm on that branch) | watched path |
| Child processes | handles registered by child_process (inferred: confirm they are in `_activeHandles`, else add) | pid, file |
| Workers | worker registry (`worker.rs`) | thread id, entry |
| Forked isolates | `ForkPool` with live forks | count |
| Native addons | `AddonRegistry` non-empty | addon path. Always OAM-SNAP0011: napi functions carry `v8::External` pointers into Rust heap memory (`napi.rs:1277`). |
| vm contexts / vm modules | `vm_module.rs:170` slot plus a new live-context counter in `vm_context.rs` | OAM-SNAP0011 in v1 (interceptor templates, `vm_context.rs:504`) |
| Ticks, microtasks, rejections | `has_pending_ticks` (`modules.rs:1382`), `RejectionLedger`, `UncaughtLedger` | an unhandled rejection at build is a failure, as at run |
| stdio | lazy stdio getters (`node_compat.js:16504`) | a *warning*, not a failure, if `process.stdout`/`stdin` was materialized: its `isTTY`/`columns` are the build's. Refuse if stdin is open or in raw mode. |
| Inspector | `InspectorSlot` | refuse |

Pooled transport state inside `CoreRuntime` (fetch keep-alive connections, TLS pipes,
h2 sessions; `oam_core/src/lib.rs:1051-1081`) is not JS-visible. It dies with the
build's tokio runtime and is **not** an error. A JS object still holding a reference
to such a resource is (inferred: confirm every such JS wrapper registers in
`_activeHandles`).

### 8.2 Things V8 itself rejects

These are found at the earliest point that is cheap, else mapped from V8's failure to
OAM-SNAP0013 with a hint:

- WebAssembly instances and memories, `SharedArrayBuffer`.
- Off-heap `ArrayBuffer` backing stores created from Rust (`ops.rs`, `node_ops.rs`,
  `napi.rs` call `new_backing_store_from_*`).

Whether V8 copies the bytes of each kind or refuses is not verified. PR 4 must test
each kind: a Buffer made at build, a Uint8Array from fetch, a wasm module.

## 9. Runtime-data refresh after deserialize, and the frozen-secrets hazard

Everything the JS heap *copied* from the process at build is wrong at run. Load order:

1. Deserialize, then the configure-isolate slots (fresh `TimerQueue`, `PendingOps`,
   `CoreRuntime`, `ProcessArgv`, permissions from the *run's* flags).
2. Refresh the **data** properties on the natives objects. `node_ops::install` puts
   values on them, not just functions: `pid`, `ppid`, `cpuCount`, `platform`, `arch`
   (`node_ops.rs:41-124`). This re-runs the data half of install. Functions are
   already in the heap (section 5).
3. Call a new `__oamNode.refreshRuntimeData()`, which resets JS-side caches:
   - `envCache` / `envShared` (`node_compat.js:15145-15160`). `process.env` is a
     Proxy over a store filled lazily on first read, so a build that read *any* env
     var has the *whole* build environment in the heap.
   - `argvCache` / `argvAssigned` / `argv0Cache` (`:15480-15500`).
   - `process.pid` / `ppid`, copied as values at `:15729-15730`.
   - `execPath`, `title`.
   - `performance.timeOrigin`, computed once at `:42252`.
   - The `ProcessArgv`-dependent caches.

   `process.cwd()` (op), `process.hrtime` (`natives.hrtimeNanos`, `:15829`) and the
   permission queries (`natives.permissionEnabled()`, `:33886`) read live and need
   nothing.
4. Run the deserialize callbacks, then the main function (or, without one, keep the
   loop running for what the build left registered, which is nothing by
   construction).

**Frozen values that cannot be refreshed.** These are documented, not fixed:

- Values the app copied: `const token = process.env.API_KEY` at top level,
  `const started = Date.now()`, a `cwd` captured into a closure, config files read at
  build.
- `Math.random` state. V8 is expected to reseed on context deserialization
  (inferred). The test plan checks that two runs from one blob differ.
- Any JS-side entropy pool. None was found by grep for the usual names, but that is
  not exhaustive and needs an audit before PR 5.

The documented pattern, as in node, is: read runtime data inside the deserialize
main function or a deserialize callback.

**Frozen-secrets hazard.** A blob is a file that may be copied, cached, attached to
bug reports or shipped. Anything in the heap at build is in the file: env-var
values, tokens read from config, request bodies. Proposed rules:

- `oam snapshot` builds with a **scrubbed environment by default** (`env_mode:
  "clean"`): only the variables the platform needs to run (`PATH`, `HOME` /
  `USERPROFILE`, `SYSTEMROOT`, `TEMP` / `TMP`, `TZ`, `LANG`), plus explicit
  `--snapshot-env NAME` passes. `--snapshot-env-inherit` restores node's behavior.
  This is a divergence from node, and an open question (Q3).
- Before serializing, `envCache` is dropped even in inherit mode, so the
  `process.env` store itself never lands in the blob. Only app-copied values can.
- The blob is written owner-only (0600 on unix; on Windows the default ACL of the
  output dir; inferred adequate), and `oam snapshot` prints one line stating that the
  blob contains the app's heap.

## 10. CLI shape

```
oam snapshot <entry> -o <blob> [--snapshot-no-code] [--snapshot-env NAME]...
             [--snapshot-env-inherit] [permission flags] [-- build-args...]
oam run --snapshot <blob> [permission flags] [-- args...]
```

- `oam snapshot` sets `v8.startupSnapshot.isBuildingSnapshot()` to true. Build args
  become the build's `process.argv` tail.
- `oam run --snapshot` takes no file positional (clap: `file` becomes optional only
  when `--snapshot` is given). `process.argv[1]` is the entry path recorded in the
  header, unless node's differential says otherwise (Q5).
- Permission flags apply per invocation. The build's grants are not baked into the
  run, and the run's grants are what the deserialized app gets.
- **Node flag mapping** on oam's node-style front end (`main.rs:300-620`):
  - `--snapshot-blob <f> --build-snapshot <entry>` maps to `oam snapshot <entry> -o <f>`.
  - `--snapshot-blob <f> [args]` maps to `oam run --snapshot <f> -- args`.
  - `--build-snapshot-config <json>` maps `builder` and `withoutCodeCache`.

  This mapping is what lets a node-differential conformance case spawn
  `process.execPath` with node's flags under both runtimes.
- **NODE_OPTIONS:** `--snapshot-blob` and `--build-snapshot` are **not** accepted
  there. The NODE_OPTIONS allowlist deliberately excludes everything that executes
  code or names a file (`main.rs:301-313`), and a blob is both. This holds even if
  node accepts them there (node: recalled, unknown). It is a documented divergence.
- `oam compile` with an embedded app snapshot is a natural follow-up (a compiled
  binary already carries bytecode, `lib.rs:965-985`) and out of scope here.

## 11. Security

- **A blob is code.** It is a serialized heap including compiled functions, and
  loading it runs them without any parse step that might have caught anything. V8
  does not authenticate it.
- **Explicit opt-in only.** It loads only from a path named on the command line of
  the current invocation (`--snapshot` / `--snapshot-blob`). It never loads from
  `OAM_CACHE_DIR`, the bytecode cache dir, the working directory, an env var,
  NODE_OPTIONS or package.json. No "auto-snapshot on second run" mode exists.
- The header's SHA-256 detects corruption, not tampering. Anyone who can write the
  blob can run code as the user who starts it, exactly like the entry `.js` file.
  The docs say so in those words. Signed blobs are out of scope (Q8).
- **Loading a malformed blob into V8.** The header checks (section 7) reject foreign
  and truncated files before V8 parses anything. Past those checks, V8's deserializer
  is not hardened against hostile input (inferred), which is why the trust boundary
  is "the user named this file".
- Reading the blob is the CLI's job, like reading the entry. It is not a JS
  `fs.read`, so `--allow-fs-read` does not gate it (same as the entry today).
- Frozen secrets: section 9.
- Any weakness found while building this is reported to Jeff privately, never in a
  public issue or commit message (repo practice).

## 12. Unsafe budget

**Expected net change: zero new `unsafe`** (`conformance/unsafe-budget.json`,
`oam_engine` ceiling 605). Each piece is safe:

- Building the table is safe: union construction, `map_fn_to`, `Function::new_raw`
  (section 5.2).
- **Blob lifetime.** `CreateParams::snapshot_blob` takes `StartupData`, and
  `StartupData: From<T: Into<Cow<'static, [u8]>>>` (`snapshot.rs:107-114`). A
  `Vec<u8>` read from the file is a `Cow::Owned`, which satisfies `'static` without
  leaking. `CreateParams` keeps it in its allocations (`isolate_create_params.rs:45-
  53`), and those live in the isolate annex until the isolate is disposed
  (`isolate.rs:937-940`). That matters because V8 reads the blob again on every
  `Context::new` (every `vm.createContext`).
- **Leak vs mmap.** Leaking (`Box::leak`) is unnecessary given the above. mmap would
  avoid one copy, but `memmap2::Mmap::map` is `unsafe`, adds a dependency (not in
  Cargo.lock today), and leaves the blob open to modification under the mapping. V8
  deserializes eagerly and touches every page anyway (inferred), so the gain is one
  memcpy. **Decision: read into a `Vec`.** Revisit only if 4a shows the read
  dominating.

The real hazard is a mismatched extref table crashing in C++. It is not a Rust
`unsafe` site, so it is not counted, and the identity hash (5.3) is its guard.

## 13. Workers and the fork pool

`worker_threads` isolates (`worker.rs:452`, `new_worker_thread_runtime`), fork-pool
prewarm isolates (`fork.rs:100`) and `oam serve` pool isolates all go through
`new_inner` with `OAM_SNAPSHOT` and no extref table. They stay that way:

- A worker started by a snapshot-loaded app runs its own entry on the built-in
  snapshot, as node's workers do (node: recalled; node's workers do not inherit the
  user-land snapshot).
- At build, a live worker or fork is refused (8.1). The fork pool is lazy
  (`lib.rs:645-663`), so an app that never forks has nothing to refuse.
- `ForkPool` in an app-snapshot process is created fresh at load like any slot.

## 14. Test plan

**Node-differential cases** (conformance corpus, against node v22.22.2). Each spawns
`process.execPath` with node's flags, so the same file runs under both:

1. In a normal run, `isBuildingSnapshot() === false`. `addSerializeCallback`,
   `addDeserializeCallback` and `setDeserializeMainFunction` throw
   `ERR_NOT_BUILDING_SNAPSHOT` (code and class). This is PR 0. Today oam's stubs
   silently no-op and lack `setDeserializeMainFunction`
   (`js/node_compat.js:33143-33147`): fake success, against house policy.
2. Build plus run of a CJS script: top-level state survives, serialize callbacks run
   once at build, and deserialize callbacks run before the main function, in
   registration order with their `data` argument.
3. `process.argv`, `process.env.X`, `process.pid` read inside the main function
   reflect the *run*. A value copied at top level reflects the *build*.
4. A second `setDeserializeMainFunction` throws
   `ERR_DUPLICATE_STARTUP_SNAPSHOT_MAIN_FUNCTION`.
5. A build whose entry throws exits with code 14.
6. `--snapshot-blob` pointing at a garbage file: compare the exit code and that
   stderr is non-empty, not the text.

**oam e2e** (`crates/oam_cli/tests/e2e.rs`):

- Each quiescence check (timer, unref'd timer, server, socket, pending fs op, child,
  worker, addon with napi feature, vm context, pending TLA, stdin raw mode) gives
  OAM-SNAP0010 or 0011, with the offender named in stderr. One test with three
  offenders asserts all three are listed.
- Mismatch: a flipped `extref_hash` byte, `base_snapshot_sha256`, `oam_version` and
  `v8_flags` each give the right code before V8 runs. A truncated file gives 0001,
  and a payload byte-flip with an intact header gives 0003.
- Secrets: build under `SECRET_TOKEN=s3kr1t-<random>` with the default env mode,
  where the entry reads `process.env` at top level and the blob does not contain the
  marker bytes. In inherit mode with the entry copying the value, it does contain
  them (documenting the hazard). In inherit mode without a copy, it does not
  (`envCache` drop).
- `vm.createContext()` in a snapshot-loaded process has only intrinsics (the pristine
  default context survived).
- Two runs from one blob give different `Math.random()` and `crypto.randomUUID()`.
- Relocation: deleting the entry after build gives OAM-SNAP0002.
- ESM phase 2: a three-module graph with a dynamic `import()` after deserialize of
  both an already-evaluated module (same namespace identity) and a new one.
- A worker started from a snapshot-loaded app runs and exits.
- The run applies `--permission` grants from the run, not the build.

**Unit:** header encode/decode round trip and every reject path. The extref table
order is stable, and the hash changes on reorder. Completeness: the xtask lint over a
fixture with a stray `Function::new`.

**Timing** (RELIABILITY.md rule: relative, generous margin, never absolute): a
generated app whose top level does measurable work starts from its blob faster than
from a warm bytecode cache. This is the acceptance criterion from the handoff doc.

## 15. Phased PRs (each at most 400 logic lines)

| PR | Content | Depends on |
|---|---|---|
| 0 | Honest `v8.startupSnapshot`: add `setDeserializeMainFunction`, throw `ERR_NOT_BUILDING_SNAPSHOT` outside a build, plus differential case 1. Ships even if 4c is shelved. | none |
| 1 | Native registry: one list per module (timers, ops, node_ops, cjs) feeding both `install` (`Function::new_raw`) and `external_references()`, the build.rs `OAM_EXTREF_HASH`, and the xtask completeness lint. No behavior change. | 4a go |
| 2 | `snapshot_blob.rs`: header format, identity, OAM-SNAP codes, unit tests. | 1 |
| 3 | `configure_isolate` refactor; `JsRuntime::build_app_snapshot` / `from_app_snapshot` (library only, no CLI) for a module-less script; Vec-backed `StartupData`; the deserialize-cost measurement goes into section 2. **Second go/no-go point.** | 2 |
| 4 | Quiescence checker plus diagnostics, serialize-callback plumbing, `isBuildingSnapshot()` true, `PendingOps` op names. | 3 |
| 5 | Runtime-data refresh, deserialize callbacks, main function, env scrub. | 4 |
| 6 | CLI: `oam snapshot`, `oam run --snapshot`, node flag mapping, CJS single-file entry (`CjsCache.main` via context data), differential cases 2-6, `cli-reference.md`. | 5 |
| 7 | ESM graph: `ModuleMap` via context data, recomputed identity hashes, dynamic import after load. | 6 |
| 8 | Docs: `node-divergences.md` (ESM extension, stricter quiescence, env scrub, NODE_OPTIONS refusal), CHANGELOG, measured numbers in section 2. | 6 or 7 |

PR 1 touches the 329-entry `bind!` list only if the macro's call shape changes. The
proposal keeps the call shape and changes the macro body, so the diff stays small.

## 16. Open questions for review

1. **Is 4c worth it at all?** Its ceiling for the motivating app is about zero
   without patching cli.js (section 1). Proceed for apps in general, or shelve after
   PR 0?
2. **ESM in phase 2:** take on module-record serialization (beyond node), or stay at
   node's single-file-CJS limit and tell ESM apps to bundle to CJS?
3. **Env scrub by default** (a divergence from node) vs node's inherit plus a loud
   warning?
4. **Payload hash on every start** vs length plus `is_valid` with `--snapshot-verify`.
   Decide from the PR 3 numbers.
5. **`process.argv[1]` shape** after deserialize: pin from node v22.22.2 before
   PR 6.
6. **Flag freezing:** node flag globals are `configurable: false` (`main.rs:2754-
   2760`). Require a match (OAM-SNAP0004, proposed), or make them configurable so the
   run's flags win?
7. **Relocatable blobs** (build root rewritten at load), or absolute paths forever?
8. **Signed blobs** for distribution, and embedding in `oam compile`: later and
   separate?
9. **Snapshot only the dependencies** (evaluate a configured set of side-effect-free
   modules, not the entry) for code-split apps like Claude Code 2.1.300: a real win,
   or a correctness trap?
10. **Error-code family name** `OAM-SNAP`, and exit code 14 for build failures.

## 17. Corrections to round-1 citations (verified while writing)

- `v8.startupSnapshot` stubs are at `js/node_compat.js:33143-33147` at `aca6d2d`,
  not `:32897-32901`. They have three methods; `setDeserializeMainFunction` is
  absent.
- `ModuleMap` is `crates/oam_engine/src/modules.rs:62-77` (round 1 said `:73-76`).
- `node_ops.rs` binds 329 natives through one macro (defined at `:128-136`, invoked
  once from `:137`; the same count at 7128e81). Two entries,
  `cryptoCipherSetAutoPadding` and `tlsSetDefaultCaCertificates`, span several lines,
  so a line-based grep finds 327; an earlier draft of this doc said 323. `cjs.rs` has 4
  function-creation sites (1 `Function::new` + 3 `Function::builder`), not 1.
- `vm_context.rs:504` (`ObjectTemplate` interceptors) was not in round 1's list. It
  also needs extref entries, or a refusal.
- The `'static` StartupData lifetime does **not** force a leak or mmap
  (section 12).
