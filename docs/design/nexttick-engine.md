# nextTick engine integration: host-driven tick points

Status: SHIPPED 2026-07-27 (GO per docs/design/streams-port.md section 11).
This is the "engine microtask-policy integration" deferred by section 4 of
that doc. All acceptance criteria in section 4 below were met; two findings
from implementation are recorded in section 5.

## 1. Problem

`process.nextTick` is a JS-side FIFO queue whose drain is scheduled as ONE
`queueMicrotask(drain)` per burst (streams-port slice 0). That gives FIFO
within a batch and batch-ahead-of-later-promise-jobs, but two residuals
diverge from Node:

1. A promise job enqueued BEFORE the first tick of a burst runs before the
   batch (Node runs ticks ahead of already-queued promise jobs at every tick
   point).
2. A tick scheduled FROM a promise-job context drains at the trampoline's
   scheduling position INSIDE the microtask checkpoint, not after the full
   microtask queue exhausts (Node: `processTicksAndRejections` loops
   drain-ticks -> run-microtasks until both are empty).

Live failure: `test-stream-compose-operator.js` throw-after-drain -- an async
generator's throw becomes an unhandled rejection instead of rejecting the
composed consumer. Error LOSS in async-iterator composition sits on the
MCP-host path (SSE / async-iterator pipelines), which is why this is worth an
engine seam.

## 2. Design: Node's tick-point loop, owned by the host

Node's model (lib/internal/process/task_queues.js): after every macrotask,

```
do { drain the tick queue to exhaustion; run a microtask checkpoint; }
while (tick queue is non-empty)
```

oam already drives V8 with explicit `perform_microtask_checkpoint()` calls
after each macrotask (timer fire, op settlement, module eval steps). The
change: every one of those checkpoints becomes the loop above.

### 2.1 JS seam (node_compat.js)

The process factory exposes two closures on globalThis when it materializes
(installRuntimeGlobals runs it at every startup, before any user code):

- `__oamDrainTicks()` -- the existing drain (FIFO, per-tick ALS binding,
  uncaughtException routing, finally-reset), minus the trampoline: nothing
  schedules it as a microtask anymore; ONLY the host calls it.
- `__oamHasTicks()` -- `tickIndex < tickQueue.length`, so the host can loop
  without a JS call sacrificing the empty fast path... (the host checks a
  fresh drain-needed signal after each checkpoint).

`process.nextTick` just pushes onto the queue. Before the process factory has
run, the globals are absent -- and no ticks can exist either, so the host
treats absent as empty.

### 2.2 Host seam (oam_engine)

`modules::run_ticks_and_microtasks(tc)` replaces every bare
`perform_microtask_checkpoint()`:

```rust
loop {
    drain_ticks(tc)?;                    // call __oamDrainTicks if present
    tc.perform_microtask_checkpoint();   // may schedule new ticks
    if !has_ticks(tc) { break; }         // ticks from promise jobs -> loop
}
```

A throw escaping the drain (the fatal no-uncaughtException-listener rethrow)
surfaces through the same diagnostic path as a throwing timer callback.

`pump_event_loop` additionally drains stranded ticks before its exit check:
if only ticks remain (no ref'd timers, no inflight ops), they run before the
loop is allowed to end -- Node's exit semantics.

## 3. Non-goals

- No V8 MicrotasksPolicy change: the policy is already effectively explicit
  (the engine owns every checkpoint).
- No Rust-side tick queue: the queue stays in JS; the host only owns WHEN it
  drains. Two cheap JS calls per tick point; revisit only if bench regresses.
- `queueMicrotask` itself, promise jobs, and the unhandled-rejection ledger
  are untouched.

## 4. Acceptance

- The 5 slice-0 next_tick e2e tests stay green (sync-context semantics are
  unchanged by construction).
- New e2e: tick-vs-already-queued-promise-job order, and tick-from-promise-
  context draining after full microtask exhaustion (the two residuals).
- `test-stream-compose-operator.js` flips to PASS -> stream bucket 160/164,
  suite 339/402; ratchet raises to 337.
- Differential case 52 gains the throw-after-drain block (deferred at slice 3
  precisely because it reddened without this fix).
- Full gate: e2e, node-suite, differential, clippy/fmt.

## 5. Implementation findings (2026-07-27)

1. **The section-3 "policy is already effectively explicit" assumption was
   WRONG.** The isolate ran under V8's default AUTO policy, which flushes the
   microtask queue whenever the API call depth reaches zero -- e.g. inside
   `module.evaluate()` -- BEFORE the host's tick drain. The fix is part of
   this change after all: `isolate.set_microtasks_policy(Explicit)` at
   creation; every checkpoint now happens only inside
   `modules::run_ticks_and_microtasks`.
2. **js/streams.js had a non-spec re-pull condition** (`waiters.length > 0`
   in addition to WHATWG's `pullAgain`): a source that defers its enqueue
   past the microtask queue (into process.nextTick -- exactly what the
   vendored-streams toWeb bridge does via resume()) re-pulled in a tight
   microtask loop, growing the tick queue to OOM once ticks stopped
   interleaving with microtasks. Fixed to pullAgain-only; the deferred
   enqueue resolves the pending waiter directly.
3. A fatal throw escaping the JS drain routes through the UncaughtLedger
   (not a direct diagnostic), preserving oam's documented divergence: a
   THROWING uncaughtException handler re-delivers and the run survives
   (pinned by e2e next_tick_survives_throwing_uncaught_exception_handler).

Shipped numbers: suite 338 -> 339/402 (84.3%), stream bucket 159 -> 160/164
(98%, compose-operator flipped), ratchet 336 -> 337, differential 55/55 with
case 52 extended by the throw-after-drain block, e2e 321 -> 323 (two new
ordering pins), zero failure-list regressions.
