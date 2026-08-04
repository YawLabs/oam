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
   Lands the `Stream` variant plus the two body ops on their first real
   caller.
3. **`IncomingMessage` as a real Readable** over the pull op, and flip the
   default once e2e is green on it.
4. **Outbound streaming + cancel.** `FetchRequest` channel variant,
   `ClientRequest.write` re-pointed.
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
- Oversize handling: buffered path still returns 413 and still reads cleanly
  on Windows; streamed path errors the request stream.
- `test-stream-pipeline.js` b008 passes; b006 tracked separately.
