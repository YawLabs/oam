// The main thread's fatal-microtask and exit-listener paths (#398, #399)
// are not a worker's. In a worker:
//
// - A queueMicrotask callback that throws with nothing to take the throw
//   does not cut the rest of that drain short: the next queued microtask
//   still runs before the worker stops, and the worker exits 1.
// - A promise job an 'exit' listener queues that throws does not change
//   the worker's exit code: it exits 0. (node also hands the parent an
//   'error' event for it; the worker's error shape is #399 items 3 and 4,
//   so the parent here prints only what it sees of the exit.)
//
// Each scenario runs as a child that starts the worker; the child's stdout
// and status are printed.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-474-"));
const files = {
  "w-microtask.cjs": `
const marker = require("path").join(__dirname, "second-ran.txt");
queueMicrotask(() => { throw new Error("wboom"); });
queueMicrotask(() => require("fs").writeFileSync(marker, "yes"));
`,
  "w-exit-job.cjs": `
process.on("exit", () => { Promise.resolve().then(() => { throw new Error("wexit-mt"); }); });
`,
  "parent.cjs": `
const fs = require("fs");
const path = require("path");
const { Worker } = require("worker_threads");
const marker = path.join(__dirname, "second-ran.txt");
const w = new Worker(path.join(__dirname, process.argv[2]), { stderr: true });
w.stderr.resume();
w.on("error", () => {});
w.on("exit", (c) => console.log("worker exit", c, "second microtask ran:", fs.existsSync(marker)));
`,
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);

try {
  for (const worker of ["w-microtask.cjs", "w-exit-job.cjs"]) {
    fs.rmSync(path.join(dir, "second-ran.txt"), { force: true });
    const run = spawnSync(process.execPath, ["parent.cjs", worker], { cwd: dir, encoding: "utf8" });
    console.log(`## ${worker} (status ${run.status})`);
    for (const line of run.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
