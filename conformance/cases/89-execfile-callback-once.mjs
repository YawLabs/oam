// A child that never starts emits BOTH 'error' and 'close' on the
// ChildProcess -- node does this too, and oam matches it. What node guards is
// the exec/execFile CALLBACK: both events route through one handler behind an
// `exited` flag, so the callback fires exactly once.
//
// oam wired 'error' and 'close' as two independent listeners, so the callback
// ran twice and the second call rebuilt an error from the close code -- which
// put the NUMBER -4058 into err.code. Any caller doing
// `err.code === 'ENOENT'` took the wrong branch on that second call, and
// callback-style "is this binary installed?" probes ran their error path
// twice. This pins the count, not just the first call's shape.
import { execFile, exec } from "node:child_process";

const MISSING = "definitely-not-a-real-binary-xyz";

// Waits for the FIRST callback, then for a quiet period, rather than racing a
// fixed timer against process spawn -- a plain setTimeout is flaky under load
// (a cold debug-build spawn can outlast it, and the case then reports 0 calls
// on a healthy runtime).
function countCalls(label, run) {
  return new Promise((resolve, reject) => {
    let calls = 0;
    const seen = [];
    let quiet;
    const safety = setTimeout(
      () => reject(new Error(`${label}: callback never fired`)),
      30000,
    );
    run((err) => {
      calls++;
      seen.push(`${typeof err?.code}:${err?.code}`);
      clearTimeout(quiet);
      // Any second call lands almost immediately after the first (the two
      // events are emitted back to back), so this only has to outlast that.
      quiet = setTimeout(() => {
        clearTimeout(safety);
        console.log(label, "calls=" + calls, "shapes=" + JSON.stringify(seen));
        resolve();
      }, 400);
    });
  });
}

await countCalls("execFile-missing", (cb) => execFile(MISSING, [], cb));
await countCalls("exec-missing", (cb) => exec(MISSING, cb));

// A binary that DOES exist and exits non-zero must still call back once, with
// a numeric exit status rather than a spawn code.
await countCalls("execFile-exit-nonzero", (cb) =>
  execFile(process.execPath, ["-e", "process.exit(3)"], cb),
);

// node stamps err.cmd on every exec/execFile error.
await new Promise((resolve) => {
  execFile(MISSING, [], (err) => {
    console.log("cmd is a string:", typeof err.cmd === "string");
    console.log("cmd names the binary:", String(err.cmd).includes(MISSING));
    console.log("errno type:", typeof err.errno);
    resolve();
  });
});
