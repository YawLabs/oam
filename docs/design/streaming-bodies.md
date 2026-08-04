# Streaming HTTP bodies

Status: SHIPPED (slices 1-5 complete, 2026-08-04). `test-stream-pipeline.js`
passes; divergence #16 in `docs/node-divergences.md` now documents only the
GET/HEAD wire-shape difference that remains.

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

   4b DONE: `ClientRequest.write` streams an incremental body. Streaming a body means reqwest cannot set
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

   **4b IS NOW LANDED** (see below). Kept for the record:

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
5. **Re-check `test-stream-pipeline`.** DONE, and the prediction in this
   doc was WRONG: with slices 1-4b landed (server dispatches on headers,
   client streams an incremental body), b008 STILL TIMES OUT and b006 still
   exits 1. Both pass on Node from the same extracted blocks, so the
   extraction is sound.

   So "b008 unblocks once slice 4 lands" was an assumption, never verified,
   and it does not hold. Whatever b008 needs is NOT simply streaming in both
   directions -- that now exists and is measured (server dispatch ~7ms,
   client dispatch ~9ms). Re-diagnose it from scratch rather than trusting
   the earlier attribution: extract the block (it starts at line 258 of the
   vendored file), run it against Node with tracing on both sides, and find
   the actual first divergence.

   NOTE for whoever does that: extract into a scratch dir, NOT into
   `conformance/vendor/node/test/parallel/` -- files there are picked up by
   the harness as new suite tests.

   FIRST DIVERGENCE, measured (b008 reduced to a traced probe -- server does
   `pipeline(req, res)`, client streams 11 chunks via `pipeline(rs, req)`):

       node: srv dispatched -> cli got response -> cli receives 10 data
             chunks -> rs.destroy() -> both pipelines settle
       oam:  srv dispatched -> srv pipeline done IMMEDIATELY err=undefined
             -> cli got response -> NO data chunks ever -> cli pipeline
             "Premature close"

   So the server-side `req` reaches EOF straight away and echoes an EMPTY
   body, which is why the client never sees the 10 chunks it waits on. The
   client IS streaming (slice 4b, verified separately at ~9ms dispatch), so
   the fault is on the RECEIVE side. INSTRUMENTED, and here is the answer:

       [pump]  frame 5 bytes ... x10   (client streams fine, hyper delivers)
       [srv]   dispatched
       [_read] chunk 5 id=2            (first read OK)
       [_read] EOF chunk=undefined     (SECOND read reports EOF)
       [pump]  consumer gone

   The pump has frames queued, yet the second read returns EOF. The cause is
   in `op_http_request_body_read`: `take_body_stream(id)` REMOVES the
   receiver from the registry for the duration of the await, and when that
   lookup misses, the op falls through to `take_request_body(id)` and returns
   `Done`. So a receiver that is merely CHECKED OUT is indistinguishable from
   a body that never existed, and the miss is reported to JS as EOF.

   FIXED (partially -- read on). `take_body_stream` now leaves a
   `StreamPending` marker and returns a `BodyCheckout` enum, so the op tells
   Ready / InFlight / Absent apart and no longer reports EOF for a receiver
   that is merely checked out. The false-EOF class is gone and the ordering
   improves: the server pipeline no longer completes before the response.

   b008 STILL FAILS THOUGH. The client still receives no data chunks and the
   server still echoes an empty body (`[srv] pipeline done err=undefined`),
   so something ELSE also ends the server-side body early. The false EOF was
   real and worth fixing on its own, but it was not the only cause -- do not
   assume this was the last one. Next: trace where the server's
   IncomingMessage reaches `complete`/`push(null)` now, since the read op no
   longer produces a premature EOF.

   THE SOMETHING ELSE, FOUND AND FIXED: `RequestGuard::drop`. The guard
   fires when `handle_request` returns -- which for a STREAMING response is
   the moment JS sends headers, not when the exchange ends -- and it removed
   the `bodies` entry unconditionally, dropping the live chunk receiver
   mid-body. So the instant `pipeline(req, res)` wrote its first chunk, the
   pump lost its consumer and the next read fell through to Absent -> EOF.
   Reproduced outside b008 with a POST echo (5 chunks, 30ms apart): node
   echoes all 5 live; oam echoed only c1. The fix, as finally landed after
   an adversarial review round tore up the first version:

   - `RequestGuard` carries a `dispatched` flag, set once the request has
     been handed to the JS accept queue. On drop, Stream/StreamPending
     entries of a DISPATCHED request survive -- whether or not a response
     was sent, because an aborted upload must deliver the pump's queued
     `Err` to the consumer (Node errors the request; reaping here silently
     truncated it to a clean 'end'). An undispatched request (queue send
     failed or cancelled mid-send) has no JS reaper, so drop removes it.
     The first version keyed on "was a response sent" (pending responder
     absent) instead -- wrong twice: a respond-vs-disconnect race leaked
     the entry with no JS path ever notified, and the cancelled-pre-
     response path swallowed the error.
   - `put_body_stream` reinserts ONLY onto a StreamPending marker, so a
     body cancelled while a read was in flight stays cancelled (dropping
     the receiver stops the pump) instead of being resurrected.
   - JS owns the reap of every dispatched streamed body: read-to-EOF and
     read-error remove the entry in the op itself; `_dumpReq` fires on
     response finish, on the `httpStreamClosed` watcher (client vanished
     mid-response), and on the respond-failed branch of `res.write()`
     (the respond-vs-disconnect race above -- the only notification JS
     gets that the exchange died); and `IncomingMessage._destroy` cancels
     on mid-stream destroy.
   - `_dumpReq` is gated on Node's resOnFinish semantics: `req._consuming
     || req._readableState.resumeScheduled`. `_read` sets `_consuming`,
     but that alone loses a RACE the e2e gate caught: a handler that does
     `req.on('data')` and `res.end()` in the same tick has only SCHEDULED
     its first read (resume runs on nextTick; the finish path on a
     microtask), so the dump fired before `_consuming` was set and
     truncated the drain. Without the gate at all, a handler that
     responds early and keeps draining had its body cancelled mid-read,
     and a client abort mid-upload was converted into a clean truncated
     'end' with `complete === true` -- the review's worst finding.
   - A handler that neither reads, responds, nor destroys leaks the entry
     (bounded: <= 8 chunks + a parked pump task) for the request's
     lifetime -- the same class as JS holding a request object forever.
     Accepted and documented rather than papered over.

   AND THE PART NOBODY PREDICTED: with full-duplex fixed, b008 does not
   even exercise it. The vendored block's client is `http.request({port})`
   -- a GET -- and Node never chunk-frames GET/HEAD writes
   (`useChunkedEncodingByDefault` = false): the request goes out on first
   write with the body bytes following UNFRAMED. A Node server parses a
   bodyless GET (req ends immediately, handler echoes EMPTY -- measured:
   the raw wire is `GET / ... \r\n\r\nhelloworld`), and the stray bytes
   poison the connection (400 + teardown), which is what settles the
   client pipeline with ERR_STREAM_PREMATURE_CLOSE. On Node, b008 passes
   via this degenerate path -- data never flows through the echo at all.
   A framed (POST) variant of b008 would fail `mustSucceed` on Node too,
   because the client-side rs.destroy() would abort a request the server
   is still reading.

   oam's client streamed a PROPERLY-FRAMED chunked body even for GET, so
   it diverged: the old GET/HEAD early-return in `_startBodyStreamIfOpen`
   meant the request never went out until `end()` -- which b008 never
   calls -- so the whole block hung. Fixed by matching Node's observable
   shape without the wire poisoning: a GET/HEAD with a body still open on
   the next tick dispatches the request BODYLESS immediately (`_sent`
   guard keeps a later `end()` from firing a second request; buffered
   writes never go on the wire). The response completing then closes a
   never-finished request -- the same premature close Node's dead
   connection produces. Divergence #16 in docs/node-divergences.md
   documents the wire-level difference.

   Three holes caught after the first version, all fixed: upgrade
   handshakes (`connection: upgrade` with a preamble write before
   `end()`) must NOT early-dispatch or `_doUpgradeRequest` never runs and
   'upgrade' never fires -- the arm branch skips them; `end(cb)`
   registered its callback via `once('response')`, which the early
   dispatch makes droppable (the response can now precede `end()`), so
   both registration sites fire the callback immediately when `this.res`
   is already set; and the premature close originally rode the
   response-reader's EOF, so a caller that never CONSUMED the response
   hung forever (the e2e gate caught this -- 2 wedged hours of it). Now a
   `_droppedWrites` request that is still un-`end()`ed when its response
   arrives is closed on the next tick, the socket-death shape, regardless
   of whether anyone reads the response.

   ORIGINAL FIX DIRECTION: keep an entry present while the receiver is checked out
   (a Stream/StreamPending distinction, or hold it under a lock instead of
   remove-await-reinsert) so a subsequent read WAITS rather than EOFing.
   Note remove-await-reinsert was copied from the accept queue, where it is
   safe -- single consumer, no fallback. The fallback added so buffered
   bodies (TLS/http2) share one JS path is what makes it unsafe here.

   Original text: blocks b008; b006 additionally needs
   the `op_http_stream_closed` ref/unref question resolved
   (`crates/oam_engine/src/node_ops.rs:1140`) -- it is deliberately unref'd
   today because ref'ing it can pin the loop forever when hyper never
   notices a vanished peer.

   OUTCOME: with the two fixes above, the ENTIRE vendored
   test-stream-pipeline.js exits 0 -- b008 and b006 both pass, and the
   ref/unref question never needed answering. node-suite moved 399/401 ->
   400/401 (the windows-aarch64 pass floor is ratcheted to 400); the only
   remaining failure is test-process-versions, the deliberate one. The
   MCP-shaped smoke (keep-alive JSON-RPC agent, including a 64KB body the
   handler never reads, then more requests on the same socket) passes.

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
  MET 2026-08-04: the whole file exits 0 (b006 included).

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

## Resuming this work

Paste-able brief for a fresh session. Read the rest of this document first --
it carries the design, what shipped, and the wrong turns, and it outranks this
summary wherever they disagree.

**State.** COMPLETE as of 2026-08-04. Slices 1-5 all landed: server
dispatches on headers, `IncomingMessage` is a real Readable, outbound body
channels exist, `ClientRequest` streams an incremental body, a streamed
request body survives the response starting (full-duplex
`pipeline(req, res)` echoes live), and GET/HEAD writes follow Node's
dispatch-on-first-write shape. `test-stream-pipeline.js` exits 0 (b008 AND
b006). node-suite 400/401 (99.8%, floor ratcheted to 400 on
windows-aarch64), e2e 324/0, conformance 55/55. The loopback latency bug
found along the way is fixed.

**Remaining follow-ups:**

- Happy-eyeballs for loopback: BLOCKED UPSTREAM, root cause fully
  identified (2026-08-04). The ~307ms a `::1`-only server pays is
  hyper-util's `happy_eyeballs_timeout` -- `ConnectingTcp::new`
  (hyper-util 0.1.20, client/legacy/connect/http.rs) parks the fallback
  address family behind a `tokio::time::sleep(300ms)` default.
  `HttpConnector::set_happy_eyeballs_timeout` exists, but reqwest 0.13.4
  exposes no path to it for h1/h2 (only its private h3 connector has an
  eyeballs impl), and reqwest accepts no custom connector. Fix is an
  upstream reqwest knob or moving the client off reqwest; a pre-probe
  connection was considered and REJECTED (every first localhost request
  would show the server a phantom accept+EOF).
- DONE 2026-08-04: the slow-consumer memory test from Risks
  (`http_streamed_body_backpressure_bounds_memory`) -- 64MB upload with a
  parked consumer: accepted writes must plateau under 16MB, RSS growth
  under 32MB, then every byte still arrives after the drain.
- GET/HEAD buffered writes are retained in `_body` until the request
  object dies; an unbounded producer piping into a GET grows it. Node
  instead writes the bytes to a (poisoned) socket. Pathological either
  way; noted in divergence #16.

**Gates.** All must pass before committing; run sequentially with
`set -o pipefail`:

    cargo test -p oam_cli --test e2e          # 324 passed, 0 failed
    cargo run -q -p xtask -- node-suite       # 399/401; the 2 failures are known
    cargo run -q -p xtask -- conformance      # 55/55, wpt 888/888 + 278/278
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

node-suite takes >10 minutes -- run it in the background rather than letting it
time out the call.

**Traps that cost real time.**

1. `js/*.js` is compiled into the V8 startup snapshot. Every JS edit needs
   `cargo build -p oam_cli` before it takes effect.
2. The Bash heredoc MANGLES backslash escapes: `\n` written inside a python
   heredoc lands as a REAL newline, breaks the JS string literal, and the
   snapshot builder dies with `STATUS_STACK_BUFFER_OVERRUN` -- an error that
   looks unrelated to what you edited. Build escapes via `chr(92)`, or edit
   line-wise. Always `node --check js/node_compat.js` afterwards.
3. Measure call -> dispatch DELTAS per request, never absolute timestamps. A
   second sequential request naturally starts ~300ms in and looks slow while
   actually being fast. This produced two false root causes in one session.
4. Extract test blocks into a scratch directory, NEVER into
   `conformance/vendor/node/test/parallel/` -- anything there is picked up by
   the harness as a new suite test and silently moves the denominator.
5. Never kill release-path `oam.exe` (those are live MCP sidecars). If a build
   fails with "failed to remove file ... Access is denied", kill ONLY processes
   whose path matches `*target\debug*`.
6. Any probe that re-spawns the runtime must guard recursion with an ENV VAR,
   never argv -- an argv guard fork-bombed the machine (7k processes).
7. `http.request` / `ClientRequest` and the http server are on the LIVE MCP
   sidecar path. Anything touching them needs an MCP-shaped smoke (JSON-RPC over
   a keep-alive agent, including a body the handler never reads), not just e2e.

Do NOT mark b008 skipped in the manifest to make the number green. It is a real
gap and is deliberately left failing. (Resolved 2026-08-04: it PASSES now --
never skipped, the gap was closed for real.)
