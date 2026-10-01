// AbortSignal.timeout(delay), three ways oam differed from node:
//
// 1. The timer kept the process alive. Node unrefs it, so a script whose
//    only pending work is a timeout signal exits at once; oam armed a ref'd
//    setTimeout, so `fetch(url, { signal: AbortSignal.timeout(5000) })` held
//    the process open for five seconds after the response had been read.
// 2. The reason's message: node says "The operation was aborted due to
//    timeout", oam said "The operation timed out".
// 3. The delay was not validated. Node runs validateUint32 on it; oam took
//    anything and returned a signal.
//
// The exit check runs a child that arms a 30 s timeout and nothing else, and
// kills it after 10 s: a ref'd timer is reported as "killed", an unref'd one
// as a normal exit. No timing value is printed.
import { execFile } from "node:child_process";

// 1) the reason.
{
  const signal = AbortSignal.timeout(1);
  console.log("before:", signal.aborted, signal.reason);
  await new Promise((r) => setTimeout(r, 50));
  const reason = signal.reason;
  console.log(
    "after:",
    signal.aborted,
    reason.constructor.name,
    reason.name,
    JSON.stringify(reason.message),
    reason.code,
    reason instanceof DOMException,
  );
}

// 2) the abort event fires, once, while something else keeps the loop alive.
{
  let fired = 0;
  const signal = AbortSignal.timeout(5);
  signal.addEventListener("abort", (ev) => {
    fired++;
    console.log("abort event:", ev.type, ev.target === signal, signal.reason.name);
  });
  await new Promise((r) => setTimeout(r, 60));
  console.log("fired:", fired);
  try {
    signal.throwIfAborted();
    console.log("throwIfAborted: no throw");
  } catch (e) {
    console.log("throwIfAborted:", e.name, e.message);
  }
}

// 3) delay validation.
for (const [label, call] of [
  ["'x'", () => AbortSignal.timeout("x")],
  ["undefined", () => AbortSignal.timeout()],
  ["null", () => AbortSignal.timeout(null)],
  ["10n", () => AbortSignal.timeout(10n)],
  ["{}", () => AbortSignal.timeout({})],
  ["-1", () => AbortSignal.timeout(-1)],
  ["1.5", () => AbortSignal.timeout(1.5)],
  ["NaN", () => AbortSignal.timeout(NaN)],
  ["Infinity", () => AbortSignal.timeout(Infinity)],
  ["2**32", () => AbortSignal.timeout(2 ** 32)],
  ["2**53", () => AbortSignal.timeout(2 ** 53)],
  ["0", () => AbortSignal.timeout(0)],
  ["2**31-1", () => AbortSignal.timeout(2 ** 31 - 1)],
]) {
  try {
    const signal = call();
    console.log(label, "->", signal.constructor.name, "aborted", signal.aborted);
  } catch (e) {
    console.log(label, "->", e.name, e.code, JSON.stringify(e.message));
  }
}

// 4) the timer alone does not hold the process open.
await new Promise((resolve) => {
  execFile(
    process.execPath,
    ["-e", "AbortSignal.timeout(30000); console.log('armed');"],
    { timeout: 10000 },
    (err, stdout) => {
      console.log(
        "child:",
        JSON.stringify(stdout.trim()),
        err ? (err.killed ? "killed (the timer held it open)" : "error " + err.code) : "exited on its own",
      );
      resolve();
    },
  );
});
