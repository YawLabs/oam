# Design: io_uring FS fast path

Status: **accepted, incremental rollout in progress** (slice 1 = file reads).
Scope: Linux-only async file I/O via io_uring, opt-in, with a runtime probe and
a full fallback to the existing path. Authored 2026-06-22.

## Motivation

oam's M3 positioning is hosting TS/JS MCP servers and CLI tools, a workload that
is **file-read-heavy** (module graphs, configs, package.json walks). Today every
filesystem op runs on tokio's blocking thread pool:

- `fs_read_file` (oam_core/src/lib.rs) calls `tokio::fs::read`, and `tokio::fs`
  is a wrapper that `spawn_blocking`s synchronous `std::fs` onto a worker thread.
- Other fs ops (`fs_readdir`, `fs_rm`, append-write, `check_access`) call
  `spawn_blocking` explicitly.

So each read costs a thread hop to the blocking pool plus a blocking syscall.
io_uring replaces that with **true async file I/O**: submit the read to the
kernel ring, get a completion, no blocking-pool thread held for the duration.
Under concurrent load (many small reads, the MCP-server pattern) this removes
both the per-op thread hop and blocking-pool contention.

This is the "win the published benchmark axes" lever for the fs-read case, and
it is explicitly an `oam serve` opt-in fast path in ROADMAP.md (M3).

## Current substrate (what we integrate with)

`CoreRuntime` (oam_core/src/lib.rs:108) owns:

- a **multi-thread tokio runtime** (`new_multi_thread().worker_threads(2)`,
  epoll reactor) — drives sockets (tcp/tls/udp/http/ws/dns) and the blocking
  pool used by fs;
- an `mpsc<OpCompletion>` channel — every async op resolves by sending an
  `OpCompletion { id, outcome }` back to the single JS thread, which the engine
  drains in `pump_event_loop`.

The op model is the key seam: **an op is just an `async fn -> OpOutcome` whose
result is delivered over the completion channel.** io_uring can be slotted in
*underneath* a specific op without changing that contract.

## The composition problem

`tokio-uring` and `monoio` are **separate runtimes**: they run an io_uring-driven
*current-thread* executor and own the thread that calls them. io_uring SQEs must
be submitted from, and CQEs awaited on, that runtime's thread. You cannot
`.await` a `tokio-uring` file op from inside a normal multi-thread-tokio task —
different reactors.

So io_uring does **not** drop into the existing runtime. It runs on its **own
dedicated thread**, and the existing tokio ops dispatch to it over a channel and
await a oneshot reply. This keeps the multi-thread tokio runtime for everything
else and adds io_uring as an additive, isolated lane.

## Design

### Dedicated io_uring thread

On Linux, `CoreRuntime` lazily starts one `oam-io-uring` thread that runs
`tokio_uring::start(async { ... })`. Inside, a loop receives `UringRequest`
messages (initially just `ReadFile { path, reply: oneshot<io::Result<Vec<u8>>> }`),
performs the io_uring op (`tokio_uring::fs::File::open` + `read_at`), and sends
the result back over the oneshot. The thread lives for the CoreRuntime's
lifetime; shutdown drops the request sender, the loop ends, the thread joins.

A persistent thread (not `tokio_uring::start` per op) is required: per-op ring
setup would cost more than it saves, defeating the purpose.

### Runtime probe + fallback (load-bearing)

io_uring is not always available: kernels < 5.1, and seccomp-restricted
environments (some containers, hardened CI) block `io_uring_setup`. The fast
path therefore **probes at startup** — attempt to create the ring / start the
thread; on failure, record "unavailable" and every fs op uses the existing
`tokio::fs` path. This makes the feature safe everywhere and, importantly,
keeps CI green even on a runner that blocks io_uring (it just exercises the
fallback).

There is also an explicit opt-in/opt-out knob (env or `oam serve` flag, TBD in
slice wiring) so the fast path can be disabled without recompiling.

### Where it plugs in

`fs_read_file(path)` becomes:

```
if uring enabled && available { dispatch ReadFile to the uring thread, await oneshot }
else { tokio::fs::read(&path).await }   // unchanged existing path
```

The returned `OpOutcome::Bytes` / `node_fail` mapping is identical, so the
engine and JS sides are untouched. Non-Linux targets `#[cfg]` the whole lane
out and only ever compile the `else` branch.

## Alternatives considered

- **monoio** (thread-per-core io_uring): a bigger paradigm shift aimed at
  thread-per-core servers; overkill for oam's single-JS-thread + op-channel
  model. Rejected for now.
- **raw `io-uring` crate**: maximum control, maximum code, manual SQE/CQE and
  ring lifecycle. More surface to get wrong write-blind. Reserve for if
  `tokio-uring` proves too limiting.
- **Swapping the socket reactor to io_uring**: larger, riskier (tokio-uring net
  is less mature), and the marginal win over epoll is smaller than the fs win.
  **Deferred** — sockets stay on tokio/epoll. FS first.

## Rollout plan

1. **Slice 1 (this change): file reads.** io_uring lane + probe + fallback,
   wired into `fs_read_file` only. Proves the architecture end to end on CI.
2. Slice 2: extend to write / open+read-chunk / stat as the win is measured.
3. Slice 3 (maybe): evaluate the socket reactor, separately.

## Verification (Linux-only code, developed via CI)

This code is `#[cfg(target_os = "linux")]` and cannot be built or run on the
Windows/macOS dev boxes — the local build cfg's it out and exercises the
fallback. It is verified on Linux **via CI**, which is a real Linux test
environment, not a stand-in:

- **Correctness:** the CI `ubuntu-latest` leg (kernel 6.x) compiles and runs the
  `#[cfg(target_os = "linux")]` tests against a real kernel. A focused test reads
  a file through the io_uring lane and asserts the bytes; the fallback path is
  covered on every platform.
- **Performance:** `bench.yml`'s ubuntu leg measures `fs-read` with the fast
  path on vs off (directional, like the rest of that matrix — shared runners).
- **Safety on restricted runners:** if a runner blocks `io_uring_setup`, the
  probe fails and the fallback runs, so CI stays green either way.

Tradeoff accepted: iteration is CI-paced (push + ~7 min) rather than local, and
CI perf is directional. Neither blocks development.

## Risks

- `tokio-uring` is 0.x; API churn / limitations may push us to raw `io-uring`.
- The dispatch hop (op thread -> io_uring thread -> oneshot back) adds latency;
  it must be a net win vs the blocking-pool hop it replaces. Measure before
  extending beyond reads.
- Kernel/seccomp variance — mitigated by the probe + fallback.
