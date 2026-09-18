# hyper 1.10.1, patched for oam

This directory is hyper **1.10.1** as published on crates.io, plus two
changes: a fix for a client hang (items 1-3 below) and one server extension
(item 4). The root `Cargo.toml` swaps it in with `[patch.crates-io]`.

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
- **Remove it when** a hyper release ships the fix **and** oam no longer
  needs item 4 (see "The request-head extension" below for what replacing
  it takes). To do that:
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
