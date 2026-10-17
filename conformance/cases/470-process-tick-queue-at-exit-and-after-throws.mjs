// When the nextTick queue runs around an exit or an uncaught throw (#366,
// #399 item 8). Seen through warnings, which are delivered on the tick
// queue, but the cause is the queue:
//
// - Once 'exit' is emitted the queue takes nothing: a warning emitted from
//   an 'exit' listener is never printed.
// - A fatal throw ends the run: a warning queued before it is not printed,
//   and after a 'warning' listener's throw the second queued warning is not.
// - A throw an 'uncaughtException' listener handles leaves the rest of the
//   batch queued, and node runs it at its next tick point: the end of the
//   next timer or immediate callback, or its check phase. An immediate
//   already queued when the throw is handled sits at that boundary, and
//   whether it runs before or after the resumed batch is node's platform
//   detail, not the queue's contract -- probed, v22.22.2, 100 runs per
//   platform, an entry-queued immediate comes before the batch on Linux and
//   macOS but after it on Windows. So the scenario queues its immediate from
//   the batch's own last tick: not there to interpose, the resumed batch
//   finishes first on every platform (probed on all three).
// - A timer due in the same timers phase runs before that rest: node's
//   timers phase goes on to its next due timer after the throw, and runs
//   the queue when the phase ends. The entry busy-waits so both timers are
//   due when the loop first looks; without that, whether the second is due
//   in the first timers phase depends on node's loop clock.
//
// The printing goes to stderr, which the harness does not compare, so each
// scenario runs as a child and its stderr is printed here: the report's
// source frame and message lines, without the stack (each runtime's own
// frames), the runtime banner, node's one-time `(Use `node --trace-...`)`
// hint (docs/node-divergences.md #51), the pid, and with paths cut to the
// file name.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-470-"));
const files = {
  // The four rows of #366.
  "warning-in-exit.cjs": `
process.on("exit", () => process.emitWarning("in exit"));
`,
  "warning-then-throw.cjs": `
process.emitWarning("pre-throw");
throw new Error("fatal");
`,
  "warning-listener-throws.cjs": `
let n = 0;
process.on("warning", (w) => { n++; if (n === 1) throw new Error("listener threw on " + w.message); });
process.emitWarning("first");
process.emitWarning("second");
`,
  "warning-listener-throws-handled.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
process.on("warning", (w) => { throw new Error("listener threw on " + w.message); });
process.emitWarning("first");
process.emitWarning("second");
setImmediate(() => console.log("immediate"));
`,
  // The same through plain ticks: where the rest of a batch runs after a
  // handled throw. The immediate is queued from the batch's last tick, not
  // from the entry: whether an immediate already queued when a throw is
  // handled interposes before the resumed batch is node's platform detail
  // (probed, v22.22.2, 100 runs each: an entry-queued immediate comes after
  // the batch on Windows but before it on Linux and macOS). Queued late it
  // is not there to interpose, so the batch finishes first everywhere.
  "main-then-immediate.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
process.nextTick(() => { throw new Error("t1"); });
process.nextTick(() => console.log("t2"));
setImmediate(() => console.log("immediate"));
`,
  "two-handled-throws.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
process.nextTick(() => { throw new Error("t1"); });
process.nextTick(() => { throw new Error("t2"); });
process.nextTick(() => { console.log("t3"); setImmediate(() => console.log("immediate")); });
`,
  "timer-then-later-timer.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
setTimeout(() => {
  process.nextTick(() => { throw new Error("tickboom"); });
  process.nextTick(() => console.log("tick2"));
}, 1);
setTimeout(() => console.log("later"), 200);
`,
  "timer-then-due-timer.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
setTimeout(() => {
  process.nextTick(() => { throw new Error("tickboom"); });
  process.nextTick(() => console.log("tick2"));
}, 1);
setTimeout(() => console.log("later"), 5);
const end = Date.now() + 50; while (Date.now() < end);
`,
  "timer-then-due-timer-same-list.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
setTimeout(() => {
  process.nextTick(() => { throw new Error("tickboom"); });
  process.nextTick(() => console.log("tick2"));
}, 1);
setTimeout(() => console.log("later"), 1);
setTimeout(() => console.log("after the phase"), 300);
const end = Date.now() + 50; while (Date.now() < end);
`,
  "timer-then-immediate.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
setTimeout(() => {
  process.nextTick(() => { throw new Error("t1"); });
  process.nextTick(() => console.log("t2"));
  setImmediate(() => console.log("immediate"));
}, 1);
`,
  "immediate-then-immediate.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
setImmediate(() => {
  process.nextTick(() => { throw new Error("tickboom"); });
  process.nextTick(() => console.log("tick2"));
});
setImmediate(() => console.log("immediate 2"));
setTimeout(() => console.log("later"), 200);
`,
  "io-then-immediate.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
require("fs").stat(__filename, () => {
  process.nextTick(() => { throw new Error("t1"); });
  process.nextTick(() => console.log("t2"));
  setImmediate(() => console.log("immediate"));
  setTimeout(() => console.log("timer"), 0);
});
`,
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);

function report(stderr) {
  return stderr
    .split(/\r?\n/)
    .filter((l) => l !== "" && !/^\s+at /.test(l) && !/^(Node\.js|oam) v\d/.test(l) && !l.startsWith("(Use `"))
    .map((l) =>
      l
        .replace(/\((node|oam):\d+\)/g, "(node:PID)")
        .replace(/(?:file:\/\/\/)?(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.[cm]?js)/g, "$1"),
    );
}

try {
  for (const file of Object.keys(files)) {
    const run = spawnSync(process.execPath, [file], { cwd: dir, encoding: "utf8" });
    console.log(`## ${file} (status ${run.status})`);
    for (const line of run.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
    for (const line of report(run.stderr)) console.log(`  err | ${line}`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
