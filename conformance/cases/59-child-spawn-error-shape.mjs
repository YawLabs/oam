// A failed spawn() must report a Node-shaped ENOENT error, and a failed spawn
// that asked for an 'ipc' slot must let the process EXIT rather than hang.
//
// Regression guard 1 -- error shape. oam handed the native spawn failure
// straight through, so err.message was literally the JSON body
// {"code":"ENOENT","message":"program not found"} and err.code / err.syscall /
// err.path were all undefined. Every "is this binary installed?" probe in the
// ecosystem is written as `if (err.code === 'ENOENT')`, and the launcher layers
// (cross-spawn, execa, npm's bin shims) read err.syscall and err.path to build
// their own message -- so the blob turned a recoverable "not installed" into an
// unrecognizable crash with a JSON document as its human-facing text. Case 26
// pins err.code alone on the extra-fd path; this pins the whole contract, and
// pins it on BOTH paths, because the ordinary spawn and the extra-fd spawn are
// shaped by different code and have regressed independently.
//
// Regression guard 2 -- no hang. With an 'ipc' entry in the stdio array oam
// binds the channel to a loopback listener and tore that listener down only
// from the child's 'exit' event -- which never fires for a child that never
// started. The listener stayed registered as a live handle, so a script whose
// only work was a failed IPC spawn printed everything it meant to print and
// then sat there until it was killed. Node exits as soon as the 'error' is
// delivered. Both stdio shapes are covered because the 'ipc' slot is spliced
// out before routing: a 4-entry array lands on the ordinary path, a 6-entry one
// on the extra-fd path, and each attaches the channel from its own call site.
//
// The "did not hang" assertion is structural, not asserted: this case registers
// no timers and holds no handles of its own, so running off the end and exiting
// IS the check. There is deliberately NO setTimeout watchdog calling
// process.exit() -- a watchdog would keep the case green against exactly the
// defect being guarded. The harness's per-leg timeout is what turns a hang into
// a reported failure.
import { spawn } from "node:child_process";

const CMD = "oam-no-such-binary-zzz";

// Node's contract for spawning a missing binary. Extra-fd stdio has a native
// backend only on win32/linux/darwin, so elsewhere the extra-fd legs print
// these same lines and node==oam still holds (same shape as case 23).
const EXPECTED = {
  code: "ENOENT",
  syscall: `spawn ${CMD}`,
  path: CMD,
  message: `spawn ${CMD} ENOENT`,
};

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";

// Resolves on 'error'. No 'exit'/'close' listener on purpose: the child never
// started, so 'error' is the only event guaranteed to arrive, and a listener
// waiting on the others is precisely the shape that hangs.
const spawnError = (stdio) =>
  new Promise((done) => {
    spawn(CMD, [], { stdio }).on("error", done);
  });

// err.errno is left out on purpose: the libuv number is platform-specific
// (-4058 on Windows vs -2 on POSIX), so pinning it would need a per-platform
// table for no added coverage. The four fields below are the ones callers
// actually branch on.
const emit = (label, e) => {
  console.log(`${label} code`, e.code);
  console.log(`${label} syscall`, JSON.stringify(e.syscall));
  console.log(`${label} path`, JSON.stringify(e.path));
  console.log(`${label} message`, JSON.stringify(e.message));
};

// 1. Ordinary path: a 3-entry stdio array, the default routing.
emit("plain", await spawnError(["pipe", "pipe", "pipe"]));

// 2. Extra-fd path: >3 entries routes to the raw extra-fd backend, which
//    reports its failure from a different call site than (1).
emit(
  "extra-fd",
  supported ? await spawnError(["pipe", "pipe", "pipe", "pipe", "pipe"]) : EXPECTED,
);

// 3. 'ipc' on the ordinary path. The slot is spliced out before routing, so
//    the four entries here collapse to three -- the interesting part is that
//    the channel opened for it must be torn down on the failure.
emit("ipc", await spawnError(["pipe", "pipe", "pipe", "ipc"]));

// 4. 'ipc' alongside real numbered fds, which does route to the extra-fd
//    backend. 'ipc' must stay LAST: oam refuses an earlier slot outright
//    (ERR_INVALID_ARG_VALUE) rather than silently renumbering the child's fds.
emit(
  "ipc+extra-fd",
  supported
    ? await spawnError(["pipe", "pipe", "pipe", "pipe", "pipe", "ipc"])
    : EXPECTED,
);

// Reached only if nothing above left a handle in the loop. Under the old IPC
// teardown bug oam printed every line up to here and then never exited.
console.log("drained after ipc spawn failures");
console.log("exit 0");
