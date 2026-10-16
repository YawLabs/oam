// import() loads and evaluates the module in jobs of its own, after the
// call has returned (#397). node's loader runs it inside a later promise
// job, in the importer's microtask drain, so:
//
// - the importer's next statement, and promise jobs it queued after the
//   call, run before the imported module's top level;
// - the module's process.nextTick callbacks run after that whole drain --
//   after the importer's continuation, from CommonJS as from an ES module,
//   and after ticks the importer queued after the call;
// - the call itself checkpoints nothing: a CommonJS importer's own
//   nextTick still runs ahead of its promise jobs, as at every CommonJS
//   tick point;
// - import() of a module whose top-level await waits on a timer settles
//   once the timer has run (oam rejected it: its hook could not wait);
// - a module awaiting an import() of the module that imports it, while
//   that one awaits it back, never settles: a top-level await deadlock,
//   exit 13.
//
// Regression guard: oam evaluated the module synchronously inside the
// import hook, with the importer's frame on the stack, and checkpointed
// microtasks there.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-480-"));
const files = {
  // The issue's first row: from CommonJS.
  "imported.mjs": `
export const log = [];
process.nextTick(() => log.push("tick"));
Promise.resolve().then(() => log.push("micro"));
`,
  "importer.cjs": `
import("./imported.mjs").then(({ log }) => {
  log.push("after import");
  setTimeout(() => console.log(log.join(" | ")), 5);
});
`,
  // The same from an ES module that awaits import() first.
  "importer-await.mjs": `
const { log } = await import("./imported.mjs");
log.push("after import");
setTimeout(() => console.log(log.join(" | ")), 5);
`,
  // The ES module row: the importer goes on after the call.
  "order-imported.mjs": `
log.push("sync");
Promise.resolve().then(() => log.push("p1")).then(() => log.push("p2"));
queueMicrotask(() => log.push("qm"));
process.nextTick(() => log.push("tick"));
`,
  "order-entry.mjs": `
globalThis.log = [];
import("./order-imported.mjs").then(() => log.push("import-resolved"));
log.push("entry-after-import-call");
Promise.resolve().then(() => log.push("entry-p"));
process.nextTick(() => log.push("entry-tick"));
setTimeout(() => console.log(log.join(" ")), 5);
`,
  // The same importer as CommonJS.
  "order-entry.cjs": `
globalThis.log = [];
import("./order-imported.mjs").then(() => log.push("import-resolved"));
log.push("entry-after-import-call");
Promise.resolve().then(() => log.push("entry-p"));
process.nextTick(() => log.push("entry-tick"));
setTimeout(() => console.log(log.join(" ")), 5);
`,
  // The call checkpoints nothing: a builtin, a module, a CommonJS file.
  "no-checkpoint.cjs": `
const log = [];
process.nextTick(() => log.push("tick"));
Promise.resolve().then(() => log.push("micro"));
import(process.argv[2]).then(() => log.push("imported"));
setTimeout(() => console.log(log.join(" | ")), 5);
`,
  "plain.mjs": "export const x = 1;\n",
  "plain.cjs": "module.exports = { x: 1 };\n",
  // A timer callback that imports: its ticks and its promise jobs.
  "from-timer.mjs": `
setTimeout(() => {
  globalThis.log = [];
  import("./order-imported.mjs").then(() => log.push("import-resolved"));
  log.push("timer-after-import-call");
  Promise.resolve().then(() => log.push("timer-p"));
  process.nextTick(() => log.push("timer-tick"));
  setTimeout(() => console.log(log.join(" ")), 5);
}, 1);
`,
  // A top-level await that a timer settles: import() waits for it.
  "tla-timer.mjs": `
export const value = await new Promise((resolve) => setTimeout(() => resolve("settled by a timer"), 1));
`,
  "tla-timer-main.cjs": `
import("./tla-timer.mjs").then((m) => console.log(m.value), (e) => console.log("rejected", e.constructor.name));
`,
  // A dynamic import cycle under top-level await: a deadlock.
  "cycle-a.mjs": `
export const fromA = "A";
export const b = await import("./cycle-b.mjs");
`,
  "cycle-b.mjs": `
const a = await import("./cycle-a.mjs");
export const sawA = a.fromA;
`,
  "cycle-main.mjs": `
process.on("exit", (code) => console.log("exit", code));
const a = await import("./cycle-a.mjs");
console.log("settled", a.b.sawA);
`,
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);
try {
  for (const [label, args] of [
    ["CommonJS importer's continuation", ["importer.cjs"]],
    ["ES module awaiting import()", ["importer-await.mjs"]],
    ["ES module going on after the call", ["order-entry.mjs"]],
    ["CommonJS going on after the call", ["order-entry.cjs"]],
    ["import() of a builtin checkpoints nothing", ["no-checkpoint.cjs", "node:fs"]],
    ["import() of a module checkpoints nothing", ["no-checkpoint.cjs", "./plain.mjs"]],
    ["import() of CommonJS checkpoints nothing", ["no-checkpoint.cjs", "./plain.cjs"]],
    ["import() from a timer callback", ["from-timer.mjs"]],
    ["import() of a module whose top-level await a timer settles", ["tla-timer-main.cjs"]],
    ["dynamic import cycle under top-level await", ["cycle-main.mjs"]],
  ]) {
    const run = spawnSync(process.execPath, args, { cwd: dir, encoding: "utf8" });
    console.log(`${label}: ${run.stdout.trim().split(/\r?\n/).join(" | ")} (status ${run.status})`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
