# Design: io_uring FS fast path

Status: **implemented (slices 1 and 2: whole-file reads and writes), opt-in,
default off, and not being extended.** It measured as not a win (see
"Measurement"), so the rollout stopped there; `OAM_IO_URING=1` engages it.
Scope: Linux-only async file I/O via io_uring, opt-in, with a runtime probe and
a full fallback to the existing path. Authored 2026-06-22; the "CI" this doc
describes was GitHub Actions, which the project no longer uses -- see
"Verification" for what covers the lane today.

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

1. **Slice 1 (done): file reads.** io_uring lane + probe + fallback, wired into
   `fs_read_file`. Reads were sequential. Proved the architecture on CI (ubuntu
   leg green).
2. **Slice 2 (done): concurrency + writes.** Each request is now
   `tokio_uring::spawn`'d (= `spawn_local` on the io_uring runtime), so multiple
   ops are in flight on the ring concurrently -- the actual point of io_uring
   over the blocking pool. Added a write fast path (`fs_write_file`, non-append:
   `File::create` + `write_all_at`; `data` moved in, no clone). Bounded by what
   tokio-uring 0.5 exposes:
   - **stat** is feasible via `tokio_uring::fs::statx` (the API exists) but
     deferred -- converting a raw `statx` to oam's Node-stat JSON is fiddly and
     write-blind-risky; do it when stat-heavy paths are shown to matter.
   - **readdir / open+read-chunk** are NOT in tokio-uring 0.5's surface
     (no getdents; chunked reads would need the open-file `FileRegistry` to hold
     io_uring `File`s -- a registry refactor). Out of scope.
3. Slice 3 (maybe): stat via statx; evaluate the socket reactor, separately.

## Measurement (results) -- io_uring is NOT a win here; stays opt-in

The `io-uring-ab` job in `bench.yml` A/Bs the same oam binary off vs on
(`bench/io_uring_ab.mjs`, a size x concurrency read sweep, median ms, ubuntu).

A first single-point run (16 KiB / concurrency 64) looked like a ~1.5x read win,
but a full **size x concurrency sweep did not reproduce it** -- and a re-sweep
after the chunking fix below confirmed it. Ratio = io_uring / baseline (>1 =
io_uring SLOWER), large-file rows, robust across both sweeps and all reps:

| size  | conc | io_uring vs baseline |
|-------|------|----------------------|
| 1 MiB | 1    | ~2.5x SLOWER         |
| 1 MiB | 8    | ~2x SLOWER           |
| 1 MiB | 64   | ~2.2x SLOWER         |

Small/mid files (4-64 KiB) are noise at this scale (0.08-2 ms on a shared
runner) -- one cell won in sweep 1 and lost in sweep 2. The **only robust signal
is that io_uring loses ~2x on large reads**, consistently.

**Conclusion:** the dedicated-thread dispatch hop (channel + worker + oneshot,
plus cross-thread coordination) costs more than io_uring saves at oam's file
sizes; tokio's blocking pool (large `std::fs` syscalls) already does well. The
read-chunk fix below did NOT change the large-file loss, which confirms the
bottleneck is architectural, not chunk count.

**Decision: io_uring stays OPT-IN (`OAM_IO_URING=1`), default OFF.** Default-on
would regress large reads ~2x. The lane is kept (correct and fallback-safe as
verified on the CI of the time; see "Verification" for today's coverage) as a foundation for a future architecture that could actually win
-- inline io_uring on the executor thread (no dispatch hop) or batched
submission -- not because the current dispatch-hop design wins. Revisit only
with such a redesign.

## Verification (Linux-only code)

This code is `#[cfg(target_os = "linux")]` and cannot be built or run on the
Windows/macOS dev boxes — the local build cfg's it out and exercises the
fallback.

**How it was verified while it was built (historical).** On GitHub Actions: the
`ubuntu-latest` leg (kernel 6.x) compiled and ran the Linux-only tests against
a real kernel, and `bench.yml`'s `io-uring-ab` job produced the measurements
above. Both are gone with GitHub Actions (see ROADMAP.md); the numbers in
"Measurement" are from those runs and have not been re-taken since.

**What covers it now.** The Linux release leg (`scripts/build-remote.sh`, an
x86_64 Linux container) compiles the lane and runs its two unit tests in
`crates/oam_core/src/io_uring_fs.rs`: a multi-chunk read, and a write-then-read
round trip. The fallback path is covered on every platform. The A/B measurement
survives as the `io-uring-ab` dispatch of `scripts/build-remote.sh`, which the
Linux leg of the manual benchmark sweep (`scripts/bench-platforms.sh`) runs as
an advisory step; no release gate runs it.

**The coverage is conditional, and says so.** The tests need a host that allows
`io_uring_setup`. Where the probe fails (an old kernel, a container whose
seccomp profile blocks the syscall) there is nothing to exercise, and each test
writes a `SKIP <test>: io_uring probe failed ...` line to stderr -- directly,
so the test harness does not swallow it -- and passes. A green leg whose log
carries those lines verified the fallback only, not the lane. Setting
`OAM_REQUIRE_IO_URING=1` for the test run turns a failed probe into a failure;
use it on a host known to allow io_uring. Whether the current release container
allows it has not been established, so the leg does not set it.

## Risks

- `tokio-uring` is 0.x; API churn / limitations may push us to raw `io-uring`.
- The dispatch hop (op thread -> io_uring thread -> oneshot back) adds latency;
  it must be a net win vs the blocking-pool hop it replaces. Measure before
  extending beyond reads.
- Kernel/seccomp variance — mitigated by the probe + fallback.
