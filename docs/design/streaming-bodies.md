# Streaming HTTP bodies

Status: proposed. Owner: unassigned. Prereq for: `test-stream-pipeline.js`,
divergence #16 in `docs/node-divergences.md`.

## The problem

Both halves of the HTTP path materialize a request body before it moves.

**Server.** `handle_request` calls `collect_body(body).await`
(`crates/oam_core/src/http_server.rs:603`) and only then sends
`IncomingRequest` to the JS queue (`:652`), stashing the collected bytes in
the `bodies` registry for JS to drain. The JS handler therefore does not run
until the last byte of the request has arrived. Measured with a chunked request that withholds
its terminator for 500ms: Node dispatches the handler at 13ms, oam at 521ms.

**Client.** `ClientRequest` accumulates writes and hands a complete body to
`globalThis.fetch` at `end()`. The wire shape (`crates/oam_core/src/lib.rs:1347`)
is `body: Option<String>` / `body_base64: Option<String>` -- fully
materialized, with no streaming variant and no in-flight cancel.

## Why this is worth doing

The conformance test is the least of it.

- A 2GB upload is a 2GB allocation today, bounded only by `MAX_REQUEST_BODY`
  (100MB) which rejects it outright. There is no way to process a large
  upload incrementally.
- A proxy cannot work: it must read the whole request before it can begin
  writing upstream, and cannot begin responding to the client until the
  upstream response is complete.
- Any request/response interleaving stalls -- an endpoint that responds to
  partial input (progress, streaming transforms, long-poll style protocols)
  cannot be expressed.

Response streaming OUT already exists (`ResponseBody::Channel`, and the
`httpRespondStream` / `httpStreamWrite` / `httpStreamClosed` ops). This work
is the inbound mirror of a pattern the codebase already runs in production,
which is the main reason to believe the shape is right.

## The hard part: the caps assume buffering

Three controls exist today and all three are written against a materialized
body. They are not incidental -- each encodes a real incident.

- `MAX_REQUEST_BODY` (100MB, `:30`) -- per-request cap.
- `DRAIN_BUDGET` (16MB, `:42`) -- after the cap is hit, drain this much more
  before replying 413. **This exists because on Windows, dropping a
  `TcpStream` with unread data in the kernel recv-buffer triggers an
  immediate TCP RST, and that RST races the 413 bytes in the send-buffer, so
  the client reads "connection reset" instead of the status.** Any redesign
  that drops the drain re-opens that bug on Windows only.
- `GLOBAL_BODY_BUDGET` (`:610`) -- aggregate retained bytes across in-flight
  requests, refunded by `RequestGuard`.

The tension: once the handler is dispatched on first byte, **413 is no longer
available** -- the handler may already have sent response headers by the time
the cap trips. Node has no cap at all and relies on backpressure. So the cap
semantics must change shape, not just move:

- Buffered path (handler never reads the stream): keep today's behavior
  exactly, including the 413 + drain.
- Streamed path (handler subscribes to body chunks): the cap becomes a
  stream error delivered to JS as `'error'` on the request, plus connection
  teardown. No 413, because the response may already be in flight.
- `GLOBAL_BODY_BUDGET` should account for *outstanding unconsumed chunks*,
  not total bytes seen, or a streaming endpoint that consumes promptly would
  be charged for the whole upload.

This is the part to design before writing code. Getting it wrong either
re-opens the Windows RST bug or removes the memory ceiling.

## Shape

**Inbound.** The seam is the `bodies` registry (`HashMap<u64, RequestBody>`)
drained by `take_request_body`, NOT `IncomingRequest.body` -- that field was
dead and is gone (slice 1). `RequestBody` is `Full(Vec<u8>)` for the common
small-body case (fast path allocation-identical) and gains `Stream(handle)`
for a streamed request. Dispatch on headers; feed chunks through a bounded
channel keyed by handle. New ops
mirroring the outbound set: `httpRequestBodyRead(handle)` (async, resolves a
chunk or EOF) and `httpRequestBodyCancel(handle)`.

Decision needed: what triggers `Stream` vs `Full`. Options are
content-length threshold, always-stream-and-buffer-in-JS, or a per-server
opt-in. Always-stream is the most Node-faithful (`req` is a Readable there)
but touches every existing handler, so it needs the e2e suite as the check.

**Outbound.** Add a streaming variant to `FetchRequest` -- a body-channel
handle backed by `reqwest::Body::wrap_stream` -- plus a cancel op so
`req.destroy()` mid-upload aborts the transfer instead of completing it.
`ClientRequest.write()` then pushes to the channel rather than appending to
`_body`, and `end()` closes it.

**JS.** `http.IncomingMessage` becomes a real Readable backed by the pull op.
`ClientRequest` already has the correct writable shape as of 685b7ce, so the
client side is mostly re-pointing `write`/`end` at the channel.

## Slices

Each is independently shippable and gated by the full chain
(`e2e` + `node-suite` + `conformance` + fmt + clippy).

1. **Inbound plumbing, buffered behavior unchanged.** DONE. The seam is not
   `IncomingRequest.body` (which was dead -- always `Vec::new()`, never
   read, now removed) but the `bodies` registry drained by
   `take_request_body`: it is now `HashMap<u64, RequestBody>` with a single
   `Full` variant, and `take_request_body` returns `None` rather than an
   empty body for a future `Stream` request. The `httpRequestBodyRead` /
   `httpRequestBodyCancel` ops moved to slice 2 -- with every request on
   `Full` they would have had no caller, and dead ops are worse than no
   ops. Verified zero-change: e2e 324/0, node-suite 399/401, conformance
   55/55, all identical to the commit before.
2. **Inbound streaming behind an opt-in**, with the cap semantics above.
   Server dispatch moves to headers-received for opted-in servers only.
   ENGINE SIDE DONE: `RequestBody::Stream` over a bounded channel,
   `pump_request_body` enforcing MAX_REQUEST_BODY cumulatively as a channel
   `Err` (no 413 -- the handler may already be responding), and the
   `httpRequestBodyRead` / `httpRequestBodyCancel` ops. Opt-in is a third
   arg to `httpServe`; absent = false, so every existing caller is
   unchanged. Measured: handler dispatches at ~8ms on a request whose body
   completes at 500ms (buffered path was 521ms; Node is 13ms).

   NOT yet done in this slice: TLS and http2 still pass `false` at their
   `handle_request` call sites, and no production JS path opts in -- the
   only caller is a probe. Both land with slice 3.
3. **`IncomingMessage` as a real Readable** over the pull op, and flip the
   default. DONE. `_read` pulls one chunk per call and lets `push()`
   backpressure pace the socket; `_dump` cancels so an unread body stops the
   pump. The read op serves a BUFFERED body as a single chunk, so TLS and
   http2 (still buffered) run the same JS path with no special-casing.

   Cap refinement found by the flip: a body that DECLARES itself over the
   cap via Content-Length is rejected before dispatch and stays on the
   buffered 413 + drain path. Only an undeclared body (chunked, or a lying
   Content-Length) can exceed mid-stream and become a stream error. Both
   413 e2e tests pass UNMODIFIED, which is the check that the guarantee
   survived rather than being redefined.
4. **Outbound streaming + cancel.** PLUMBING DONE (4a): `OutboundBodies`
   registry, `FetchRequest.body_stream`, `fetch` handing reqwest the
   receiving half via `Body::wrap_stream`, and the new/write/end/cancel ops
   (`write` resolves only once the chunk is accepted, so JS backpressure
   follows the socket). Needs the `stream` feature on reqwest. Verified: a
   request whose body does not exist yet goes out at ~22ms and streams
   chunks written 400ms apart.

   NOT done (4b): `ClientRequest.write` is still buffered and nothing in
   production opts in. Streaming a body means reqwest cannot set
   Content-Length and falls back to chunked -- a WIRE change on the live MCP
   client path -- so the flip is its own step and must preserve the declared
   length when it is known, the same way slice 3 was separated from slice 2.

   **4b was attempted and reverted; read this before retrying.** The JS half
   works: arming a microtask on first `write()` and streaming only if the
   body is still open on the next tick correctly keeps the common
   `write()+end()` materialized (Content-Length preserved) and streams the
   incremental case (chunked, body intact) -- measured, both correct. What
   does NOT work is the latency, which is the entire point: the server saw
   headers at ~316ms, i.e. only after `end()`, versus Node's ~36ms.

   The cause is NOT reqwest, NOT the channel, and NOT the fetch wrapper.
   Measured, all three are fine:

   - raw `__oam.fetch` with `body_stream`: server dispatched at ~7ms
   - `globalThis.fetch(url, { __oamBodyStream })`: dispatched at ~5ms
   - `globalThis.fetch` has NO await before the native call on the plain
     path (the connect-lookup branch is the only one, and it is not taken)

   An earlier revision of this note blamed the wrapper. That was inference,
   not measurement, and the A/B above refutes it -- do not start there.

   Also cleared since: `self._headers` is `{}` for a plain http.request and
   passing it changes nothing, and `self._url` is set in the constructor
   (node_compat.js:14300), so it is available when streaming arms at 3ms.

   MEASUREMENT PITFALL, which cost one wrong conclusion here: these probes
   print ABSOLUTE elapsed time, and a second sequential request naturally
   starts ~300ms in, so it LOOKS slow while actually dispatching ~1ms after
   its own call. Measure per-request deltas (call -> dispatch), never
   absolute stamps, or you will "find" a delay that is not there.

   **RESOLVED: slice 4b was never broken.** Instrumenting inside
   `_doFetchRequest` (the step this note asked for) showed it calls
   `globalThis.fetch` at +4ms with the handle set, exactly as designed. The
   ~313ms was the probe's URL: the test client used the default host
   (`localhost`) while the server bound `127.0.0.1`. Re-run against
   `127.0.0.1` and the same 4b code dispatches at **+6ms**.

   So 4b is implementable as written -- arm on first `write()`, stream only
   if the body is still open next tick, keep `write()+end()` materialized.
   It should be re-applied and taken through the full gate chain.

   The delay was a SEPARATE, REAL BUG, unrelated to streaming: see
   "localhost resolution" below.

   Historical note on the earlier text here:
   `a.write('hello')` at ~0ms produced a server dispatch at ~316ms. That
   measurement was real but MIS-ATTRIBUTED to streaming; it was the
   localhost penalty.

   Until that is understood, flipping ClientRequest buys chunked encoding
   with none of the latency win, which is a strictly worse wire trade on the
   MCP path.
5. **Re-check `test-stream-pipeline`.** Blocks b008; b006 additionally needs
   the `op_http_stream_closed` ref/unref question resolved
   (`crates/oam_engine/src/node_ops.rs:1140`) -- it is deliberately unref'd
   today because ref'ing it can pin the loop forever when hyper never
   notices a vanished peer.

Slices 1-3 are the bulk. Slice 4 is smaller but touches `fetch`, which is on
the live MCP client path.

## Risks

- **Windows RST regression** if the drain path is disturbed. There is an e2e
  test sized deliberately to stay under the drain ceiling -- keep it.
- **Live MCP sidecars run on this exact server and client.** Every slice
  needs the sidecar smoke, not just the suites.
- **Backpressure bugs are silent.** An unconsumed body channel that grows
  without bound is the failure mode this work is supposed to prevent; the
  bounded channel and the budget accounting are the guard, and both need a
  test that asserts memory does not grow with a slow consumer.

## Acceptance

- A chunked request withholding its terminator dispatches the handler within
  a few ms, matching Node (the 521ms vs 13ms measurement above is the
  before-number).
- A large upload streams with bounded memory -- a slow-consumer test shows
  flat RSS.
- Oversize handling: a declared-oversize body still returns 413 and still
  reads cleanly on Windows (it never reaches the streamed path); an
  undeclared body that exceeds mid-stream errors the request stream.
- `test-stream-pipeline.js` b008 passes; b006 tracked separately.

## Adjacent bug found while diagnosing slice 4b: localhost resolution

oam's FIRST request to `localhost` costs ~317ms; Node's costs ~6ms. Measured
on the same process, same server, plain `http.get`, no streaming involved:

    oam    127.0.0.1: 4ms    localhost: 317ms   (then 1ms / 0ms cached)
    node   127.0.0.1: 9ms    localhost: 6ms     (then 0ms / 1ms)

One-time per process, then cached, which is why it hid so well -- and why it
masqueraded as a streaming defect for two rounds of diagnosis. The shape
(a few hundred ms, first call only) points at an IPv6-first attempt to `::1`
waiting out a timeout before falling back to `127.0.0.1`.

PARTIALLY FIXED: the reqwest client now resolves `localhost` IPv4-first
(`resolve_to_addrs`, with ::1 retained as a fallback). IPv4 loopback went
317ms -> 2ms, matching Node.

The cost is MOVED, not removed, and that is worth knowing before touching
this again: a server bound only to `::1` now pays the fallback delay
instead, ~307ms where Node does it in ~10ms. That is the rarer case, so the
trade is a net win, but it IS a regression for IPv6-only loopback.

The real fix is a bounded happy-eyeballs delay rather than an ordering
preference -- reqwest 0.13 exposes no `happy_eyeballs_timeout`, so it needs
either a custom connector/resolver or an upstream knob. Node avoids both
tails via autoSelectFamily with a 250ms attempt timeout.
