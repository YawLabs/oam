# node: compat wave 1 — known-divergence punch list

Source: adversarial review fleet over commit 4d90904 (four finders running
empirical oam-vs-node parity batteries, findings verified per-item). The
blocker and every `important` finding were fixed in the follow-up commit;
this file tracked the `minor` parity gaps that remained, so they were filed
rather than forgotten.

**Re-triaged 2026-11-10.** Most of the original list has since been fixed and
was still reading as open here, which hid the entries that are not. Every item
was re-checked: the JavaScript-visible ones by running the same snippet on
oam 0.19.5 and node v22.22.2 (windows-aarch64) and comparing the output, the
native ones by reading the code they name. "Open" below means it still differs
from Node; "Closed" lists what the original entry said and is kept only so a
reader of an old link can see where it went. Deliberate, permanent differences
live in [node-divergences.md](../node-divergences.md), not here.

## Open

### N-API (alpha boundaries)

- Instance data (`napi_set_instance_data` / `napi_get_instance_data`) is not
  exported; an addon importing it fails at load with a missing-symbol error.
- `napi_open_handle_scope` / `napi_close_handle_scope` are not real scopes:
  they return a sentinel and values live in the caller's scope, so an addon
  that opens scopes in a long loop to bound memory does not get that bound.

### CLI

- Script arguments need `--`: `oam run file.ts -- --flag a` (cargo
  convention). Node passes everything after the script path, and
  `oam run file.ts a` is rejected as an unexpected argument. Documented in
  `docs/cli-reference.md`; revisit if it bites adopters.

## Closed since the list was written

Each of these now matches Node on the check described above, with one
exception: the three entries under "Fixed after the 0.19.5 check" still
differed on oam 0.19.5 and were fixed in the tree afterwards. Those three are
closed by reading the fix, not by re-running a released binary.

### Fixed after the 0.19.5 check

- `buf.indexOf(str, fractionalOffset)` never matched
  (`Buffer.from('abcabc').indexOf('abc', 0.5)` was `-1`). The offset is now
  truncated toward zero, as Node does, so that call returns `0`.
- `atob` stripped Unicode whitespace (a U+00A0 inside the input was removed and
  the rest decoded). It now strips ASCII whitespace only -- tab, LF, FF, CR,
  space -- and throws `InvalidCharacterError` on the others, as Node does.
- `fs.exists` invoked its callback synchronously. The callback is now
  validated and the answer is an `fs.access(F_OK)` delivered on a later tick,
  as Node's is.

### Buffer / encodings

- Hex decode stops at the first invalid pair ('1x', ' 1', '+1').
- Numeric `write*` methods throw `ERR_OUT_OF_RANGE` for out-of-range values and
  fractional offsets.
- 'ascii' encode is byte-identical to latin1.
- `TextDecoder` non-fatal mode emits one U+FFFD per maximal subpart of a
  truncated sequence.
- From the parity batch: `isEncoding(undefined)`, `Buffer.from(str, '')`,
  `lastIndexOf(str, negativeOffset)`, `toString` with a negative start,
  `Buffer.compare` on plain `Uint8Array`s, `encodeInto` on a short
  destination.

### path / util / assert / events

- UNC share root in `basename` / `dirname` / `relative`.
- `normalize()` keeps the trailing separator when the result collapses to '.'.
- `extname('..')`, `parse('..')`, `parse('.foo').dir`.
- `path.format()` inserts the missing dot before `ext`.
- `basename(p, suffix)` when base === suffix.
- `deepStrictEqual` compares symbol keys and distinct boxed primitives.
- `util.inspect` / `format`: `-0`, `%s` with a custom `toString`, quoting of
  strings inside objects.
- `util.types.isProxy` is native.
- `EventEmitter`: `removeAllListeners` emits 'removeListener'; `listenerCount`
  honours the listener argument.

### fs / os / process / module

- `Stats` has `nlink`, a real `mode` and its own `ctimeMs`; `fs.constants` has
  the `O_*` open flags.
- fs errors carry `.errno`, `.syscall` and `.path`, sync and async.
- `writeFileSync` / `writeFile` throw `ERR_INVALID_ARG_TYPE` for a payload
  that is not a string or a view.
- `\\?\UNC\server\share` shows as `\\server\share`, as Node's `StringFromPath`
  does (`win32_shown_path`, crates/oam_core/src/lib.rs).
- A missing path argument throws `ERR_INVALID_ARG_TYPE`, and a non-string path
  whose `toString` throws gets Node's result, the `toString` never called.
- A cross-device rename reports `EXDEV` on Linux and macOS.
- A panicking op future no longer hangs the event loop: spawned ops run under
  `catch_unwind` and complete with an error (crates/oam_core/src/lib.rs).
- A rejection whose handler is attached late (after the macrotask boundary) is
  reported and exits 1, as in Node.
- `process.nextTick` runs before already-queued promise microtasks in CommonJS
  and after them at ESM top level, as in Node; the queue is host-drained
  (docs/design/nexttick-engine.md).
- `os.release()`, `os.version()`, `os.cpus()[i].model` and `os.uptime()`
  (system uptime) are real.
- `module.createRequire('file://server/share/x.js')` returns a require
  function.
- An environment entry or a command-line argument that is not valid Unicode
  no longer ends the process: `process.env` and `process.argv` carry it
  decoded lossily (U+FFFD for what does not decode). A script path that does
  not decode is still not runnable (it is reported as not found).

### URL

- The `protocol` setter can move a special scheme to `file:` (`http:` ->
  `file:`).
- `pathToFileURL` on a '+'-bearing name gives the same href as Node.

### N-API

The surface is no longer the ~39-symbol core. Present now: `napi_wrap` and
`napi_define_class`, `napi_create_reference`, `napi_create_async_work` and
threadsafe functions, buffers / arraybuffers / typedarrays, bigint, dates and
external values. What is still missing is under "Open" above.

### HTTP server (from the M2 safety fleet)

Nothing open. The fleet's blockers and the cheap leaks are FIXED (body/pending
drop guard, serialized streaming writes, AbortController, timer-heap sweep, fs
destroy race, key copy), and so are the three design-shaped items below. The
two "remaining gap" notes inside them are closed too and are kept as written:
streaming *request* bodies shipped 2026-08-04 (`IncomingMessage` is a real
Readable over a pull op; docs/design/streaming-bodies.md), and
'unhandledRejection' listeners receive the promise as their second argument.

- ~~**No global request-body memory budget / connection cap.**~~ FIXED:
  a 512MiB aggregate retained-body budget (`GLOBAL_BODY_BUDGET`, an
  `AtomicUsize` reserved at collect-time, refunded on `RequestGuard` drop)
  layered over the 100MiB per-request cap, plus a 256-slot connection
  semaphore (`MAX_CONNECTIONS`) on the accept loop. Over-budget uploads
  get 503 (413 only on the per-request cap); excess connections are
  dropped. (0.16.3 removed the 256-connection semaphore: a few hundred
  held connections locked every other client out. Node's timeouts close
  idle and stalled connections instead, and `server.maxConnections` is
  the opt-in limit, as in Node.) The gap this entry used to end on --
  request bodies buffering fully -- closed when streaming request bodies
  shipped (see the note above). (http_server.rs)
- ~~**Streaming-response pump wedges on mid-stream client disconnect.**~~
  FIXED: `http_body_push` now wraps the `ChannelBody` send in a 60s
  `STREAM_PUSH_TIMEOUT`. A client that stops reading no longer parks the
  JS pump forever — the send either lands, errors (client gone -> Failed
  "client disconnected"), or times out (Failed "stream stalled: client is
  not reading") and ends the stream. (http_server.rs)
- ~~**server.close() hard-resets in-flight requests.**~~ FIXED: the conn
  task now drives hyper's `graceful_shutdown` on the close signal
  (`disable_keep_alive`, then let the live request finish) instead of
  dropping the connection. Byte-identical to Node's drain-then-close on
  an in-flight slow response. (http_server.rs)
- ~~process.on('uncaughtException' / 'unhandledRejection') never fire.~~
  FIXED (commit 25229e4): a present listener now suppresses the fatal
  exit and receives the real Error; no-listener stays fatal. The promise
  argument, missing then, is passed now.
