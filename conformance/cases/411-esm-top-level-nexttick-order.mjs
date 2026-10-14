// process.nextTick against promise jobs queued in the same code (#345). An
// ES module is evaluated inside a microtask drain, so at its top level the
// promise jobs -- and the code after a top-level await they settle -- run
// before any nextTick callback, in the main module, in a worker's, and in a
// module an ES module import()s. In CommonJS and in a timer callback ticks
// run first, as at every other tick point. (A module CommonJS import()s is
// left out: oam evaluates it inside the import hook, and its ticks run
// before the importer's continuation -- #397, docs/node-divergences.md.)
//
// And on the way out, as node: an ES module entry that throws -- at its
// top level, or after a top-level await -- runs no tick before the
// uncaughtException ladder, whose origin for it is 'unhandledRejection'; a
// fatal microtask throw runs no tick after it; a nextTick from an 'exit'
// listener never runs, the promise jobs those listeners queue do. A
// top-level await that never settles exits naturally, code 13.
//
// Regression guard: oam ran an ES module's top-level ticks ahead of its
// promise jobs.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-411-"));
const body = `
const log = [];
process.nextTick(() => log.push("tick"));
Promise.resolve().then(() => log.push("micro"));
queueMicrotask(() => log.push("micro2"));
`;
const esm = `${body}
await null;
log.push("after await");
await new Promise((r) => setTimeout(r, 5));
console.log(log.join(" | "));
`;
const cjs = `${body}
setTimeout(() => console.log(log.join(" | ")), 5);
`;
const timer = `
setTimeout(() => {
  const log = [];
  process.nextTick(() => log.push("tick"));
  Promise.resolve().then(() => log.push("micro"));
  queueMicrotask(() => log.push("micro2"));
  setTimeout(() => console.log(log.join(" | ")), 5);
}, 0);
`;
// A module import() loads: its own top level, and what the importer sees.
const imported = `
export const log = [];
process.nextTick(() => log.push("tick"));
Promise.resolve().then(() => log.push("micro"));
queueMicrotask(() => log.push("micro2"));
`;
const importer = `
const { log } = await import("./imported.mjs");
log.push("after import");
await new Promise((r) => setTimeout(r, 5));
console.log(log.join(" | "));
`;
const throws = `
process.nextTick(() => console.log("tick"));
Promise.resolve().then(() => console.log("micro"));
throw new Error("boom");
`;
const throwsLate = `
process.nextTick(() => console.log("tick"));
await null;
console.log("after await");
throw new Error("boom");
`;
const origin = `
process.on("uncaughtException", (e, origin) => console.log("uncaughtException", origin, e.message));
process.nextTick(() => console.log("tick"));
throw new Error("boom");
`;
const microThrow = `
process.on("exit", (code) => console.log("exit", code));
process.nextTick(() => console.log("tick"));
queueMicrotask(() => { throw new Error("boom"); });
`;
const throwsAfterTimer = `
process.on("exit", (code) => { console.log("exit", code); Promise.resolve().then(() => console.log("micro in exit")); });
await new Promise((r) => setTimeout(r, 1));
Promise.resolve().then(() => console.log("micro"));
process.nextTick(() => console.log("tick"));
throw new Error("boom");
`;
const cjsFatalExit = `
process.on("exit", (code) => {
  console.log("exit", code);
  Promise.resolve().then(() => console.log("micro in exit"));
  process.nextTick(() => console.log("tick in exit"));
});
Promise.resolve().then(() => console.log("micro"));
throw new Error("boom");
`;
// An op completes in the same turn as the timer that resumes the await.
const sameTurn = `
import fs from "node:fs";
process.on("exit", (code) => console.log("exit", code));
await new Promise((r) => setTimeout(() => {
  fs.readFile(new URL(import.meta.url), () => console.log("read callback"));
  const end = Date.now() + 200;
  while (Date.now() < end);
  r();
}, 1));
process.nextTick(() => console.log("tick"));
throw new Error("boom");
`;
const unsettled = `
process.on("exit", (code) => {
  console.log("exit", code);
  process.nextTick(() => console.log("tick in exit"));
  Promise.resolve().then(() => console.log("micro in exit"));
});
await new Promise(() => {});
`;
const exitListener = `
process.on("exit", () => {
  process.nextTick(() => console.log("tick in exit"));
  Promise.resolve().then(() => console.log("micro in exit"));
  console.log("exit");
});
`;
// A worker whose entry is an ES module.
const worker = `
import { Worker } from "node:worker_threads";
new Worker(new URL("./order.mjs", import.meta.url));
`;
const files = {
  "order.mjs": esm,
  "order.cjs": cjs,
  "timer.cjs": timer,
  "imported.mjs": imported,
  "importer.mjs": importer,
  "worker.mjs": worker,
  "throws.mjs": throws,
  "throws-late.mjs": throwsLate,
  "origin.mjs": origin,
  "micro-throw.mjs": microThrow,
  "exit-listener.cjs": exitListener,
  "throws-after-timer.mjs": throwsAfterTimer,
  "cjs-fatal-exit.cjs": cjsFatalExit,
  "unsettled.mjs": unsettled,
  "same-turn.mjs": sameTurn,
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);
try {
  for (const [label, file] of [
    ["ES module top level", "order.mjs"],
    ["CommonJS top level", "order.cjs"],
    ["timer callback", "timer.cjs"],
    ["import() from an ES module", "importer.mjs"],
    ["worker with an ES module entry", "worker.mjs"],
  ]) {
    const run = spawnSync(process.execPath, [file], { cwd: dir, encoding: "utf8" });
    console.log(`${label}: ${run.stdout.trim()}${run.status === 0 ? "" : ` (status ${run.status}) ${run.stderr.trim().split("\n")[0]}`}`);
  }
  // Exits that fail: what ran (stdout) and the status; the report itself
  // names the file each runtime's way.
  for (const [label, file] of [
    ["ES module entry that throws", "throws.mjs"],
    ["ES module entry that throws after await", "throws-late.mjs"],
    ["origin of an ES module entry's throw", "origin.mjs"],
    ["fatal microtask throw", "micro-throw.mjs"],
    ["'exit' listener's tick and promise job", "exit-listener.cjs"],
    ["ES module entry that throws after a timer", "throws-after-timer.mjs"],
    ["CommonJS throw: what runs after", "cjs-fatal-exit.cjs"],
    ["top-level await that never settles", "unsettled.mjs"],
    ["throw after a timer with an op done in its turn", "same-turn.mjs"],
  ]) {
    const run = spawnSync(process.execPath, [file], { cwd: dir, encoding: "utf8" });
    console.log(`${label}: ${run.stdout.trim().split(/\r?\n/).join(" | ")} (status ${run.status})`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
