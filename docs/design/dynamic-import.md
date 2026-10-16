# import(): node's loader chain in promise jobs

Status: implemented (#397). Code: `js/esm_loader.js` (the chain),
`crates/oam_engine/src/modules.rs` (the hook and the `op_dyn_*` ops it runs on).

## The problem

oam's import hook (`dynamic_import_callback`) used to do all the work inside the call: resolve,
load the graph, instantiate, evaluate, then run a microtask checkpoint so a synchronous graph's
evaluation promise had settled before the hook returned. The module's top level therefore ran
with the importer's frame on the stack, before the importer's next statement, and the checkpoint
ran the importer's own queued promise jobs mid-statement.

node's `import()` (v22.22.2, `lib/internal/modules/esm/loader.js` and `module_job.js`) resolves
the specifier inside the call and does everything else in promise jobs: `getModuleJobForImport`
awaits the resolution, each `ModuleJob` links its requests through further jobs,
`_instantiate` walks the graph's `linked` promises, `run` awaits `instantiate()` and then the
evaluation promise. The module evaluates inside the importer's microtask drain, so its
`process.nextTick` callbacks run after that drain, and the turn every step lands on is fixed by
the chain's shape: a module with no imports evaluates on the 8th microtask turn after the call
and `import()` settles on the 14th; each level of static imports below it adds turns; a module
already loaded settles on the 9th; a resolution failure rejects on the 5th.

## Why a JS chain

The turns are observable (case 481 counts them), and they come from the exact sequence of
`async` functions, `await`s and promise plumbing in node's loader -- including
`SafePromiseAllReturnArrayLike` over each module's requests and the recursive
`addJobsToDependencyGraph`. A native state machine that counts hops would have to reproduce
that arithmetic for every graph shape (depth, diamonds, already-loaded subgraphs, failures at
each step) and would drift the first time a shape was missed. Keeping node's chain in JS, with
the same functions in the same order, gets the turns right by construction: the promise jobs
V8 queues are the same jobs node's V8 queues. The work at each step is oam's, behind a small op
table:

| op | node step | work |
|---|---|---|
| `resolve(spec, referrer)` | `#cachedDefaultResolve` | `ModuleHost::resolve`, keyed as the module map keys |
| `checkType(path, type, spec)` | `validateAttributes` (in load) | throw unless `type` is absent, or `json` for a .json file |
| `load(path, spec)` | `loadAndTranslate` | `compile_module` |
| `requests(path)` | `getModuleRequests()` | the compiled module's requests |
| `link(path, specs, paths)` | `module.link(modules)` | record the module map's edges |
| `instantiate(path)` | `module.instantiate()` | `instantiate_module` |
| `evaluate(path)` | `module.evaluate()` | `Module::evaluate`, its promise |
| `namespace(path)` | `getNamespace()` | the namespace object |
| `isLoaded` / `isInstantiated` / `loadedDependencies` | (the load cache) | adopt a module the static graph loaded |

A module oam loaded outside the chain (the entry's static graph, a facade it imports) is one
node's loader holds a linked, instantiated job for; the chain adopts a job of that shape for it,
with its dependencies' jobs, so a new module that imports the static graph walks it on node's
turns. Every miss of the load cache runs `checkType` first, adopted or not: node's cache is keyed
by `type` too, so `import(x, { with: { type: "json" } })` of a loaded JavaScript module misses it
and reaches load, which rejects.

`ModuleJob.prototype` has a null prototype, as in node: `getModuleJobForImport` is async and
returns a job, so its promise reads `job.then`, which a polluted `Object.prototype.then` must not
answer.

The hook hands back a promise resolved with the chain's, not the chain's own: node's `import()`
settles one thenable link after its loader's promise does (a patched `Promise.prototype.then`
sees four calls per import in node, and in oam now).

The loader is a factory in the startup snapshot. The runtime calls it once at construction,
before any user code, with the op table; it captures the intrinsics it uses then (as node's
primordials do) and the runtime keeps the returned function in an isolate slot and deletes the
global, so user code can neither reach nor replace it. Its job cache is keyed per module map
(`ModuleMap::generation`), so a new run on the same runtime starts empty.

## The two hazards the issue named

**The ActiveHost pointer is only valid during the synchronous call.** No op keeps it: each one
that needs the host (`resolve`, `load`) reads the slot when it runs, through
`with_active_host`, and the reference does not outlive the op. The ops run from JS -- the hook,
and the promise jobs the chain queues -- so they run inside some microtask checkpoint. Every
checkpoint that can run them is either inside the `execute_*` call that parked the pointer (its
whole event loop runs there) or on a path that clears the slot before running JS (`tick`,
`repl_eval`, `execute_script`, `run_registered_tests`). An op therefore finds the live pointer or
None; with None it throws the old "not wired up on this entry path" error, which rejects the
import. A job left over from a run whose `execute_*` returned early reads whatever the slot
holds when it runs, never the old pointer.

**The sync-graph fast path relied on the mid-hook flush.** There is no fast path left to feed:
nothing evaluates inside the hook, so nothing needs a checkpoint there, and `run` simply
awaits the evaluation promise, synchronous graph or top-level await alike. That also retires
the old machinery for a module found mid-evaluation (`PendingCycleImports`,
`PendingTLAImports`, the "top-level await is not supported yet" rejection): a job runs between
module bodies, never inside one, so `evaluate` always meets a module that is linked and not on
the stack (the op still refuses the two states V8 would abort on). A cycle under top-level
await is now a deadlock, as in node (exit 13); a cycle without one settles.

## What stays different

- CommonJS runs as oam loads it (to name its facade's exports), on turn 1 of `import()` and
  before its ES siblings in a static graph; node runs it as the graph evaluates. Recorded in
  docs/node-divergences.md.
- node rewrites the message of a missing named export of a CommonJS module, resolving and
  loading it again to find out; oam keeps V8's message but keeps the two awaits, so the
  rejection lands on node's turn.
- Customization hooks and `diagnostics_channel` tracing are not implemented, so the chain is
  node's with neither registered.
- Load failures reject with oam's `Error` and message, as before, not node's error classes.
- A graph `import()` loads is no longer prefetched by the worker pool: modules load one per
  job, as node loads them.
