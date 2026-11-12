# js/ — runtime-internal JavaScript

The JS half of the runtime: plain JavaScript (no TypeScript, no build step of its own)
that `crates/oam_engine/build.rs` evaluates at BUILD time into the V8 startup snapshot.
`JsRuntime::new()` deserializes that context instead of parsing and running these files
at every start; the native bindings (`globalThis.__oam`) are installed after the restore.

## Files

The list and its order live in `js_files` in `crates/oam_engine/build.rs`; a file that is
not listed there is not in the binary. In evaluation order:

| File | What it holds |
|---|---|
| `bootstrap.js` | The web surface: fetch and its Response/Headers shapes, the web classes, the error factories the engine calls. |
| `esm_loader.js` | `import()`'s loader chain, a factory the runtime calls once at construction. |
| `internal/process/per_thread.js` | `process.execve`, compiled under the origin `node:internal/process/per_thread`. Must precede `node_compat.js`, which picks its factory up off `globalThis`. |
| `node_compat.js` | The `node:` builtins. Each one is a factory registered on `__oamNode` and instantiated at first import/require; the pure-JS globals (Buffer, TextEncoder/TextDecoder, atob/btoa, setImmediate) install at snapshot evaluation. |
| `vendor/oam-shims/` | The loader (`loader-prelude.js`) and the `internal/*` shims (`primordials.js`, `errors.js`, `validators.js`, `util.js`) the vendored streams need, then `seal.js` and `register.js`. |
| `vendor/node-streams/` | Node's own `lib/` stream sources, bodies byte-identical to upstream. `UPSTREAM` records the tag, commit and per-file sha256. |
| `streams.js` | WHATWG streams: ReadableStream / WritableStream / TransformStream, TextEncoderStream / TextDecoderStream. |
| `test_runner.js` | The `oam:test` module. |
| `permissions.js` | The `oam:permissions` module. |
| `ai.js` | The `oam:ai` module. |
| `mcp.js` | The `oam:mcp` module. |
| `undici.js` | The undici-API shim that the bare specifier `undici` resolves to (`oam:undici`). |

## Snapshot rules

- **No natives at evaluation time.** The snapshot context holds only pure JS. Anything
  from `__oam` must be looked up at CALL time, never captured at the top level of a file.
- **The top level registers; it does not run.** A builtin is a factory on
  `__oamNode.factories`, called at first import/require. Runtime-data globals (`process`,
  `performance`, the upgraded console) install after the restore, not here.
- **No runtime state in the heap.** Whatever the top level computes is frozen into the
  binary, so it must not read the clock, start a timer, or depend on the build host.
- **Order is load-bearing.** A file may use only what the files before it defined (the
  comments in `js_files` say which dependency pins each position).
- **Vendored files are wrapped, not edited.** Each `vendor/node-streams` body is
  evaluated inside `__oamVendor.define(<node's specifier>, function (require, module,
  exports, process, primordials) { ... })`, which only stores the factory; the body runs
  at the first runtime require. `build.rs` fails the build when a `require('...')` in one
  of them has no define, prelude shim or public fallback. `seal.js` must stay the last
  define-emitting entry.
- **The default context stays pristine.** The runtime context rides in the snapshot as an
  extra context, so a `vm.createContext` sandbox gets V8's intrinsics and nothing else.
