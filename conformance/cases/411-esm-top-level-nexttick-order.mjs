// process.nextTick against promise jobs queued in the same code (#345). An
// ES module is evaluated inside a microtask drain, so at its top level the
// promise jobs -- and the code after a top-level await they settle -- run
// before any nextTick callback, in the main module, in a worker's, and in a
// module an ES module import()s. In CommonJS and in a timer callback ticks
// run first, as at every other tick point. (A module CommonJS import()s is
// left out: oam evaluates it inside the import hook, and its ticks run
// before the importer's continuation -- #397, docs/node-divergences.md.)
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
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
