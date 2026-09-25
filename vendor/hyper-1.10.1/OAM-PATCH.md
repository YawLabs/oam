# hyper 1.10.1, patched for oam

This directory is hyper **1.10.1** as published on crates.io, plus a fix
for a client hang (items 1-3 below), one server extension (item 4), and
four stricter rules in the chunked-body decoder (items 5 to 8), a
CONNECT request read without a body (item 9), and the `host` field kept out
of an HTTP/2 request (item 10). The root `Cargo.toml` swaps it in with `[patch.crates-io]`.

- **Upstream:** `hyper-1.10.1.crate`, sha256
  `55281c53a1894c864990125767da440a4e630446785086f52523b20033b74498`
  (`OAM-PATCH.sha256`). That is the checksum Cargo.lock recorded before the
  swap; a path dependency gets none.
- **The patch:** `OAM-PATCH.diff`, against the unpacked crate.
- **The gate:** `scripts/check-vendor.sh`, run by `scripts/ci-local.sh`
  step 11. It takes the published crate (cargo's download cache, else
  static.crates.io), checks the sha256, applies `OAM-PATCH.diff`, and fails
  unless the result equals this directory byte for byte (the `OAM-PATCH.*`
  files aside). With `--build` it also compiles this copy, warning-free, for
  each feature set in `OAM-PATCH.features`. Nothing else gates this
  directory: it is outside the workspace (no fmt, clippy or tests) and outside
  the unsafe-budget scan. After an edit here, `scripts/check-vendor.sh
  --regen` rewrites the diff; review it and commit it with the edit.
- **Remove it when** a hyper release ships the fix and items 5 to 10, **and**
  oam no longer needs item 4 (see "The request-head extension" below for what
  replacing it takes). To do that:
  1. Delete this directory.
  2. Delete the `[patch.crates-io]` entry and the `exclude = ["vendor"]` line
     from the root `Cargo.toml`.
  3. Run `cargo update -p hyper`.
  4. Keep `crates/oam_core/tests/http_client_stale_pool.rs`. It has to pass
     on the release that replaces this copy.

## The diff

Everything else in this directory is upstream's. `OAM-PATCH.diff` is the
whole of it.

1. **`src/client/dispatch.rs`, `impl Drop for Receiver`.** After the existing
   `taker.cancel()`, the drop now:
   - closes the request channel;
   - drains it until tokio reports it closed **and** idle, using
     `recv()` wrapped in `tokio::task::coop::unconstrained` and polled with
     `now_or_never`;
   - drops each envelope it drains, and yields the thread on `Pending`.

   Dropping an envelope answers its callback with the error the channel
   already uses for this case: `Canceled("connection closed")`, with the
   request handed back.
2. **`src/common/mod.rs` and `src/common/task.rs`.** `common::task` (where
   `now_or_never` lives) was compiled only with `http1`, but `Receiver::drop`
   is client code for HTTP/1 and HTTP/2 alike, so `client` + `http2` without
   `http1` did not compile. The module is now built for any `client` build
   (and for `server` + `http1`, as before), and its `yield_now`, which only
   the h1 dispatcher uses, is gated on `http1` so no build warns.
3. **`Cargo.toml`, `[dependencies.tokio]`.** The version changes from `"1"` to
   `"1.44"` and the features from `["sync"]` to `["sync", "rt"]`. The fix
   needs `tokio::task::coop::unconstrained`, which needs both. oam already
   builds tokio 1.52.3 with `rt`, so the build graph is unchanged: the same
   packages with the same features. `Cargo.toml.orig` is left as upstream
   shipped it.
4. **`src/ext/mod.rs` and `src/proto/h1/role.rs`, `RawRequestHead`.** A new
   public extension type, built with `server` + `http1`, and one line in
   `Server::parse` that inserts it into every HTTP/1 request's extensions:
   the head's bytes as received, from the request line through the blank
   line. It is a clone of the `Bytes` slice the parse already holds (the
   header values point into the same buffer), so nothing is copied. See
   "The request-head extension" below.
5. **`src/proto/h1/decode.rs`, `read_trailer` and `read_end_cr`.** A bare LF
   in the trailer section of a chunked body is an error ("Invalid trailer:
   bare LF" / "Invalid chunk end: bare LF") instead of a trailer byte. See
   "Chunked trailers" below.
6. **`src/proto/h1/decode.rs`, `read_size`.** Whitespace after a chunk size
   is an error ("Invalid chunk size line: Invalid Size") instead of linear
   white space; the `SizeLws` state and `read_size_lws` are gone, and the
   crate's own `test_read_chunk_size` cases for it now expect the error. See
   "Chunk size whitespace" below.
7. **`src/proto/h1/decode.rs`, `read_extension`.** A chunk extension is read
   to llhttp's grammar ("Invalid chunk extension" otherwise) instead of
   skipped up to the CR: five `ChunkedState` variants after `Extension`, one
   `read_extension` for all six, and an `is_token` helper. The crate's
   `test_read_chunk_size` extension cases that break the grammar now expect
   the error, and gain cases for it. See "Chunk extensions" below.
8. **`src/proto/h1/decode.rs`, `decode_trailers`, and `src/proto/h1/conn.rs`.**
   A trailer section carrying `Content-Length`, or a request's carrying
   `Transfer-Encoding`, is an error ("Invalid trailer: ..."), and a repeated
   trailer field keeps all its values (`append`, where hyper's `insert` kept
   the last). The chunked decoder learns whether it reads a request
   (`Decoder::in_request`, set from `T::is_server()` where `conn.rs` builds
   it). New crate test `test_decode_trailers_framing_and_repeats`. See
   "Trailer framing fields" below.
9. **`src/proto/h1/role.rs`, `Server::parse`.** A CONNECT request's body
   length is zero, whatever `Content-Length` or `Transfer-Encoding` it
   carries; `test_decoder_request` gains the cases. See "CONNECT" below.
10. **`src/proto/h2/client.rs`, `ClientTask::poll`.** A request going out
    over HTTP/2 loses its `host` field, and in an HTTP/1.1 request being
    converted here that field names the authority `:authority` carries. One
    new private function, `authority_from_host`, and one line after the
    existing `strip_connection_headers` call. See "The host field on h2"
    below.
11. **`src/proto/h1/role.rs`, `Server::parse`.** The request-head parser
    allocates its header buffer on demand instead of the full `max_headers`
    upfront. oam raises a server's `max_headers` to a large byte-derived cap
    (so a head is refused on its size, like node, not on a field count), and
    the stock code heap-allocated that whole cap on every parse. It now starts
    with the inline capacity and grows -- re-parsing the buffered head -- only
    when a head carries more fields, up to the cap; the cap still bounds it, so
    the accept/reject boundary is unchanged and hyper's own `max_headers` tests
    still hold. See "On-demand header buffer" below.

## Why

A request can hang forever when it is sent onto a pooled connection at the
moment that connection's task finishes (bug L-3). This is what happens:

1. A server answers a request and then closes the connection. This is the
   normal case for a keep-alive timeout, or for a 302 followed by a FIN.
2. hyper-util checks the idle connection out of its pool. `Sender::try_send`
   passes `giver.give()` and pushes an `Envelope{request, Callback::Retry}`
   into the tokio unbounded channel.
3. On another worker, at the same time, the connection's h1 dispatcher reads
   the FIN, finishes with `Dispatched::Shutdown`, and is dropped.
4. The stock `Receiver::drop` only calls `taker.cancel()`. tokio's `Rx::drop`
   then closes the channel and drains it with `list.pop`. `pop` stops at a
   slot that a sender has claimed but not yet written. So if the send's
   `inc_num_messages` came before the close, but its write came after the
   drain looked, the envelope stays in the channel.
5. Only `Chan::drop` would drop the envelope and fire the "connection
   closed" callback that hyper-util retries on. `Chan::drop` needs the last
   sender, and hyper-util's `try_send_request` holds that sender while it
   awaits this same callback.

The result is a cycle with no socket and no timer, so `client.request` never
returns. For oam, the `fetch()`, `http.request` or `undici.request` built on
that request never settles.

The fix works because tokio's `recv()` on a closed channel returns `None` only
once the channel has no message in flight. So the drain also waits out a send
that is between reserving its slot and publishing into it. That window is
synchronous code in `UnboundedSender::send`, so the wait is bounded.

`unconstrained` is required. Without it, a task whose coop budget is used up
gets `Pending` from `recv()` on every poll, and the loop would spin forever.

`Receiver::drop` is the one place this can be fixed for every path, because
every client connection owns one `dispatch::Receiver`:
- the h1 dispatcher, when it finishes on an EOF, an error, or an upgrade;
- the h2 `ClientTask`'s `req_rx`;
- a connection future dropped before it finishes, for example at runtime
  shutdown.

hyper-util, like any caller that spawns the connection future, drops that
future as soon as it completes.

## The request-head extension (item 4)

oam's http server applies node's rules for a request head on top of
hyper's parser (`crates/oam_core/src/http_head.rs`): Content-Length together
with Transfer-Encoding, a duplicate Content-Length, a coding after
`chunked` and a bare-LF line ending are refused with 400, and node's
`maxHeaderSize` count is enforced with 431. Those checks need the head as
it arrived, because `Server::parse` drops two of the headers they look at
before a service sees the request: a `content-length` that follows
`transfer-encoding` is skipped (`if is_te { continue; }`), and a second
`content-length` with the same value is skipped too. Nothing in hyper's
public API exposes the raw head, so the patch adds the extension.

It only adds information: parsing, framing, keep-alive and every existing
extension are unchanged, and a caller that never reads it sees stock hyper.
hyper 1.11.0 changed the CL + TE case (#4124: the `content-length` is now
removed and the connection closed), which still leaves nothing for a caller
to detect it by, so a move to 1.11.x keeps this hunk.

It is exercised by `crates/oam_cli/tests/http_server_wire.rs` and the
conformance cases 131 and 132 (a stock hyper does not compile with oam, since
oam names the type).

## Chunked trailers (item 5)

The chunked decoder reads the trailer section byte by byte until CR LF CR LF,
treating a bare LF as an ordinary byte, and then hands the buffer to
`decode_trailers`, whose `httparse::parse_headers` stops at the first empty
line it sees -- and httparse accepts a bare LF as a line end. So in
`0\r\nX: a\n\nGET /x HTTP/1.1\r\nHost: a\r\n\r\n` the decoder read up to the
final CR LF CR LF, the parse returned after `X: a`, and the bytes between --
here a complete second request -- were consumed and dropped, with the
connection kept alive. A front end that ends the trailers at the bare-LF
blank line forwards that second request and then expects a response the
server never sends. node refuses the body (400). With the patch the body
fails at the bare LF and hyper closes the connection; the request handler
sees a body error. Only a bare LF is refused: a CR must still be followed by
LF, as before. The decoder is shared with the client, where a response
carrying such trailers now fails too, as it does in node.

Tested by `crates/oam_cli/tests/http_server_wire.rs`
`a_bare_lf_in_the_trailers_fails_the_request` (fails on stock 1.10.1).
hyper 1.11.1's decoder is unchanged here.

## Chunk size whitespace (item 6)

hyper accepted spaces and tabs between a chunk size and the CRLF or `;`
that follows it (`3 \r\n`, `3\t;ext\r\n`), reading them as linear white
space. RFC 9112 allows none there, and parsers split on it: some reject the
line, some stop at the space, some take it as hyper did. A front end that
frames a chunked body differently from the server is the request smuggling
pattern the other request rules here close, and node refuses the line (400).
With the patch the size line fails at the whitespace; oam's server answers
400 and closes the connection, as node does, and the handler sees its
request body fail. The decoder is shared with the client, where a response
with such a size line now fails too.

node's `insecureHTTPParser` accepts the whitespace; oam refuses it in both
modes (hyper has no per-connection switch for it).

Tested by `crates/oam_cli/tests/http_server_wire.rs`
`a_malformed_chunk_size_line_is_answered_like_node` and conformance case 134
(both fail on stock 1.10.1). hyper 1.11.1's `read_size` is unchanged here.

## Chunk extensions (item 7)

hyper read nothing of a chunk extension: every byte between the `;` and the
CR was skipped (counted against its 16 KiB extension limit, a plain LF
refused). llhttp, node's parser for requests and responses alike, reads the
extension to a grammar and refuses a body whose extension breaks it:

- after each `;`, a name of token characters (it may be empty, but may not
  start with SP or CR), then `;`, `=` or the CR;
- after `=`, a value of token characters and quoted strings, which may be
  empty; after a closing quote only `;` or the CR;
- in a quoted string, HTAB, SP and 0x21-0xFF except `"` and `\`, and quoted
  pairs of `\` and HTAB, SP, VCHAR or obs-text.

Whitespace anywhere else, separators such as `,` `/` `=` `@` in a name or
value, control characters and DEL are errors; `insecureHTTPParser` does not
relax any of it. Two parsers that disagree about a chunk line can disagree
about where the body ends, which is the request-smuggling pattern items 5
and 6 close. With the patch, oam's server answers such a body 400 and closes
the connection, as node does, and `fetch` / `http.request` fail a response
with one, as node's do. The extension limit is unchanged and still counts
every extension byte.

Tested by conformance case 137 (35 request lines and 6 response lines,
identical to node; fails on stock 1.10.1) and the crate's
`test_read_chunk_size`. hyper 1.11.1's `read_extension` is unchanged here.

## Trailer framing fields (item 8)

hyper took every field of a chunked body's trailer section as an ordinary
one (its own `decode_trailers` carries a TODO to disallow these). llhttp
refuses a `Content-Length` field there in any message and, in a request, a
`Transfer-Encoding` field: they are framing fields, and a front end that acts
on one frames the connection differently from a server that ignores it. With
the patch oam's server answers such a request 400 and closes the connection,
as node does, so nothing after it is read as a request; `fetch` and
`http.request` fail a response whose trailers carry `Content-Length`, as
node's do, and still accept `Transfer-Encoding` there, as node's do.
`insecureHTTPParser` (which lets node accept both) does not relax it.

hyper also stored trailer fields with `HeaderMap::insert`, so a repeated one
kept only its last value; `append` keeps them all, which oam needs for node's
`req.trailers`.

Tested by conformance case 138 (http and https, identical to node; fails on
stock 1.10.1) and `test_decode_trailers_framing_and_repeats`. hyper 1.11.1
already keeps repeated trailer fields (`append`), but still accepts the
framing fields; its `read_extension` (item 7) is unchanged from 1.10.1.

## CONNECT (item 9)

node's parser (llhttp) takes every CONNECT as an upgrade: the bytes after its
head belong to the tunnel, and the server hands them to its 'connect'
listener as `head` with the socket. oam's server does the same by taking the
connection out of hyper once the CONNECT head is parsed (`Connection::
into_parts`, with hyper's read buffer). hyper read a body for a CONNECT that
declared one, so those bytes went into a body channel and were lost to the
tunnel. With the patch the request has no body and the bytes stay in hyper's
read buffer. hyper 1.11.1's `Server::parse` is unchanged here.

Tested by conformance case 139 (`CONNECT with a length`) and
`test_decoder_request`.

## The host field on h2 (item 10)

RFC 9113 8.3.1: a client that generates HTTP/2 requests directly carries the
authority in `:authority`, and a request carrying a `Host` field that
disagrees with it is malformed. hyper fills `:authority` in from the request
URI (`h2::frame::Pseudo::request`) and forwards the header map as it stands,
so a `host` field in that map goes out beside the pseudo-header as a second
copy of it -- hyper-util adds no `host` of its own on h2, but every caller
that builds an HTTP/1.1-shaped request does. In oam that is every
`http.request` / `https.request`: node's `ClientRequest` sets a Host header in
its constructor (`getHeader('host')` reads it back and `removeHeader('host')`
drops it, which the proxy agents depend on), and oam's transport forwards the
header list verbatim.

Google's frontends answer a request carrying both with a connection reset:
before this hunk, every `https.request` to `www.google.com`,
`generativelanguage.googleapis.com`, `storage.googleapis.com`,
`oauth2.googleapis.com` (and so every axios / got / node-fetch request to
them) failed `ECONNRESET`, while `fetch` and `undici.request` -- which build
their own header lists and never carry a `host` -- were fine. The general
hazard is the one the RFC names: an origin that routes on `:authority` and a
front end that routes on `Host` can be made to disagree about where a request
is going.

With the patch the field is removed, and which of the two survives follows
how the request was written:

- An **HTTP/1.1 request being converted here** (`Version::HTTP_11`: what
  hyper-util's pooled client sends and what it upgrades when ALPN picks h2,
  and how oam's fetch path builds every request -- `http::Request::new` in
  `http_client::send`) wrote its authority in the `host` field, so
  `:authority` becomes that. curl applies the same rule converting `Host:`,
  and Go's http2 transport sends `:authority` from `Request.Host` and skips
  the field ("Host is :authority, already sent"). In the ordinary case the
  field already agrees with the URI -- node's Host header is the host, plus
  the port unless it is the scheme's default, which is exactly how `url::Url`
  spells the authority `prepare::to_uri` hands hyper -- so nothing changes
  but the field's removal. Where they disagree, the caller's HTTP/1.1 routing
  override reaches the server instead of being dropped in silence.
- A request **the caller already wrote as an HTTP/2 one**
  (`Version::HTTP_2`, which is how `h2_session::build_request` builds what
  `http2.connect` sends) authored its own pseudo-headers, so its
  `:authority` stands and only the field goes.

A CONNECT's `:authority` is the destination of its tunnel, not a routing
hint, so nothing moves that either. The connection is already open when this
runs, so nothing here can move where a request goes.

One divergence follows, on the `http2.connect` API alone: node sends a
request carrying BOTH a `:authority` and a disagreeing `host` as written
(`prepareRequestHeaders` fills `:authority` in only when neither was given),
where oam sends the authored `:authority` and drops the field. That pair is
the one the RFC calls malformed, and oam did not match node there before this
hunk either: it sent both fields where node, given only a `host`, sends only
that.

Tested by `crates/oam_core/tests/http_client_transport.rs`
`an_h2_request_sends_the_authority_without_a_host_field`,
`an_h2_host_header_that_overrides_the_authority_becomes_it` and
`an_authored_h2_request_keeps_the_authority_it_wrote` (all three fail on
stock 1.10.1), with `an_h1_request_keeps_the_host_header_it_was_given` next to
them for the other direction. hyper 1.11.1's `ClientTask::poll` is unchanged
here, and hyperium/hyper has no issue open for it.

## Reproduction

The regression tests are in `crates/oam_core/tests/http_client_stale_pool.rs`.
All counts below are from 2026-09-18.

- `a_send_racing_the_dispatcher_teardown_is_answered` runs the exact
  interleaving in memory: an idle connection reads a FIN and finishes, then
  one thread drops it while another sends on it.
  - Stock 1.10.1: the test failed 5 of 5 runs on Windows arm64 (309 to 525
    of 20,000 requests stranded) and 5 of 5 runs on an M-series Mac (15 to
    66 stranded).
  - Patched: 0 stranded, both platforms. That covers 300,000 extra rounds
    on Windows.
- `a_request_racing_a_closing_pooled_connection_settles` is the end-to-end
  shape, through oam's transport and a loopback server that answers and then
  sends a FIN.
  - Stock: failed about 1 run in 5 on each platform.
  - Patched: never failed.
- `oam run` on a loop of 20 redirects answered `302` then FIN, 500 processes
  6 at a time, on Windows arm64:
  - `main` before the patch hung 14 and 16 processes in two passes;
  - patched: 0 hung.
- A single process sending 20,000 such fetches, 6 runs: before the patch 3
  and 6 of 6 hung (two passes); patched: 0.

## On-demand header buffer (item 11)

`Server::parse` sizes two `SmallVec`s to hold the head's parsed headers -- one
of `httparse::Header`, one of `HeaderIndices`. Stock 1.10.1 sizes them to
`h1_max_headers` when it is set (`Some(cap) => smallvec![uninit; cap]`) and to
the inline `DEFAULT_MAX_HEADERS` (100) otherwise. oam sets `max_headers` from
the server's `maxHeaderSize` byte budget -- thousands of fields -- so a head is
refused on its size the way node's is, not at a 100-field wall. With the stock
allocation that would heap-allocate thousands of slots on *every* request head,
including the overwhelming majority that carry a handful of fields.

The patch starts each parse with `DEFAULT_MAX_HEADERS.min(hard_cap)` slots (the
inline 100, so no heap for the common head) and, on `httparse::Error::
TooManyHeaders` while the buffer is still under the cap, doubles it -- up to the
cap -- and re-parses the buffered head. `hard_cap` is `h1_max_headers.unwrap_or
(DEFAULT_MAX_HEADERS)`, so the accept/reject boundary is exactly what it was:
`None` still rejects past 100, `Some(0)` rejects any header, `Some(n)` accepts
up to `n`. The re-parse reads the same buffered bytes (nothing is consumed until
after the loop), and only a head that overflows 100 fields pays for the growth.
No new `unsafe`: the uninitialized slots are still read only up to the count
httparse reported. hyper's own `Server::parse` header tests (default 100, a
limit of 0, a limit of 200) exercise the boundary and still pass.

With the server path no longer using `smallvec_inline!`, that import is gated
on `client` -- its one remaining user, `Client::parse` -- so a server-only
build stays warning-free. `scripts/check-vendor.sh --build` compiles every
feature set in OAM-PATCH.features and is where an unused import in one of
them shows up.

## Upstream status (checked 2026-09-18)

- The newest hyper is **1.11.1**. Its `Receiver::drop` is unchanged, and so is
  master's. Neither 1.11.0 nor 1.11.1 lists a fix for this.
- **hyperium/hyper#4122** is open. It reports the same tokio reserve-then-
  publish window on the h1 handshake-error path, as a request body that is
  dropped late.
- **hyperium/hyper#4150** is an open PR for #4122 and has had changes
  requested. It replaces `close()` + `try_recv()` in the h1 dispatcher's
  error path with a `close_and_recv()` that spins until the in-flight
  envelope lands. It does not touch `Receiver::drop` or the clean
  EOF-on-idle shutdown, which is the path oam hits. Its spin also polls
  `recv()` under the coop budget.

A draft comment for #4122/#4150 is kept outside the repository, for a
maintainer to post.
