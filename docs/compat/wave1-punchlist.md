# node: compat wave 1 — known-divergence punch list

Source: adversarial review fleet over commit 4d90904 (four finders running
empirical oam-vs-node parity batteries, findings verified per-item). The
blocker and every `important` finding were fixed in the follow-up commit;
this file tracks the `minor` parity gaps that remain, so they are filed
rather than forgotten. None of these blocks the wave-1 packages we target;
fix opportunistically or when a real package trips one.

## Buffer / encodings (js/node_compat.js)

- Hex decode leniency overshoots Node: pairs with a valid first nibble and
  invalid second ('1x', ' 1', '+1') decode as a one-digit value instead of
  terminating the parse.
- Numeric write* methods silently wrap out-of-range values and accept
  fractional offsets where Node throws ERR_OUT_OF_RANGE.
- 'ascii' encode masks bytes with 0x7f; Node's ascii encode is
  byte-identical to latin1 (mask 0xff).
- TextDecoder non-fatal mode emits one U+FFFD per BYTE of a truncated
  sequence instead of one per maximal subpart (WHATWG/Node emit one).
- Parity batch: isEncoding(undefined)===true; Buffer.from(str,'') throws;
  lastIndexOf(str, negativeOffset) returns -1; indexOf(str,
  fractionalOffset) never matches; toString negative start is
  relative-from-end; Buffer.compare rejects plain Uint8Arrays; encodeInto
  may leak partial bytes; atob accepts Unicode whitespace.

## path / util / assert / events (js/node_compat.js)

- UNC share root: basename returns '', dirname/relative diverge on
  '\\\\host\\share' inputs.
- normalize() drops the trailing separator when the result collapses to '.'.
- extname('..') returns '.' and corrupts parse('..'); parse('.foo').dir is
  '.' instead of ''.
- path.format() does not insert the missing dot before ext (Node >= 19
  does).
- basename(p, suffix) keeps the base when base === suffix; Node strips it.
- deepStrictEqual ignores symbol keys; distinct boxed primitives compare
  equal.
- util.inspect/format: -0 prints as '0'; %s ignores a custom toString;
  strings inside objects are not escape-quoted.
- util.types.isProxy always returns false (no native hook yet).
- EventEmitter: removeAllListeners never emits 'removeListener';
  listenerCount ignores the optional listener argument.

## fs / os / process / module (js/node_compat.js + natives)

- fs.exists invokes its callback synchronously (zalgo).
- fsPromises.mkdir({recursive:true}) resolves to undefined instead of the
  first created path.
- Stats objects lack nlink; fs.constants lacks the O_* open flags.
- Async fs rejections carry .code only — .syscall/.path are dropped, and
  .errno is missing on both sync and async paths.
- writeFileSync/writeFile coerce non-string, non-view payloads through
  ToString — Node throws ERR_INVALID_ARG_TYPE.
- strip_unc_prefix mangles \\?\UNC\ network paths (strips to
  'UNC\server\...'); realpath on network shares is wrong.
- Pending exceptions from a throwing toString are replaced by the natives'
  own TypeError; missing args coerce to the string 'undefined'.
- node_error_code cannot produce EXDEV — cross-device rename surfaces as
  EIO, defeating the standard copy+unlink fallback.
- process.env / process.argv natives abort on non-Unicode environment
  entries (std::env::vars panics) — switch to vars_os with lossy decode.
- A panicking op future hangs the event loop: inflight never decrements
  and recv() blocks forever — wrap spawned ops in catch_unwind or send a
  poisoned completion on drop.
- Unhandled-rejection policy diverges from Node: late-attached handlers
  un-flag (Node warns immediately at the macrotask boundary), detection
  happens only at end of run.
- process.nextTick relative ordering vs already-queued microtasks differs
  (oam's nextTick IS a microtask; Node drains the nextTick queue at its
  own checkpoints — observable in ESM top-level where the module job
  itself is a microtask). Both-before-timers holds in both runtimes.
- stat results hardcode mode: 0 and alias ctimeMs to mtimeMs.
- os.release()/os.version() return '' and cpus() reports model 'unknown'
  without a doc note; os.uptime() is process uptime, not system uptime.
- module.createRequire mishandles UNC file:// URLs (file://server/share).

## URL (from the url-parity fleet; everything else it found was fixed)

- protocol setter cannot transition INTO 'file:' (rust-url's set_scheme
  excludes file from its special-scheme transitions; http->file no-ops
  where Node rewrites). Upstream limitation; revisit on url-crate bump.
- pathToFileURL keeps '+' literal in hrefs (round-trips fine; href
  string equality with Node differs on '+'-bearing names).

## N-API (alpha boundaries)

The alpha ships the value/property/function/error core (~39 symbols) —
enough for procedural addons. Not yet implemented (addons needing these
fail at load with a missing-symbol error naming the gap):

- napi_wrap / napi_define_class / instance data (the class machinery
  better-sqlite3-style addons need)
- napi_create_reference / deleted-value lifetimes
- napi_create_async_work / threadsafe functions (libuv-shaped)
- buffers/arraybuffers/typedarrays across the boundary
- bigint, dates, external values
- napi_open/close_handle_scope are not yet real (values live in the
  caller's scope)

## HTTP server (from the M2 safety fleet)

The fleet's blockers and the cheap leaks are FIXED (body/pending drop
guard, serialized streaming writes, AbortController, timer-heap sweep, fs
destroy race, key copy). The three design-shaped items are now also FIXED
(see below); the remaining gap is streaming *request* bodies, deferred.

- ~~**No global request-body memory budget / connection cap.**~~ FIXED:
  a 512MiB aggregate retained-body budget (`GLOBAL_BODY_BUDGET`, an
  `AtomicUsize` reserved at collect-time, refunded on `RequestGuard` drop)
  layered over the 100MiB per-request cap, plus a 256-slot connection
  semaphore (`MAX_CONNECTIONS`) on the accept loop. Over-budget uploads
  get 503 (413 only on the per-request cap); excess connections are
  dropped. Remaining gap: request bodies still buffer fully — streaming
  request bodies (so a large upload never buffers at all) is the deferred
  follow-up. Until then a reverse proxy with body limits remains the
  belt-and-suspenders deployment. (http_server.rs)
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
  exit and receives the real Error; no-listener stays fatal. Remaining
  gap: unhandledRejection passes only the reason, not the promise arg.

## CLI

- Script flags arrive via `oam run file.ts -- --flag` (cargo convention);
  Node passes everything after the script path. Documented, revisit if it
  bites adopters.
