// A microtask that throws with nothing to take the throw ends the run
// right there (#398): node exits at it, so no later promise job of the
// same drain runs -- not a sibling .then, not a queueMicrotask callback,
// not an async function's continuation -- from an ES module and from
// CommonJS alike, at the entry's drain and at a timer's. The ladder runs
// once: 'uncaughtExceptionMonitor' hears the throw one time.
//
// An 'uncaughtException' listener that throws is fatal the same way, exit
// 7, and the report's source frame is the listener's `throw`, as node's is
// (#399 item 5).
//
// Each scenario runs as a child; its stdout and status are printed, and its
// stderr as the report's source frame and message lines (no stack, no
// runtime banner, paths cut to the file name).
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-471-"));
const throwing = `
queueMicrotask(() => { throw new Error("qm-throw"); });
Promise.resolve().then(() => console.log("p-after"));
`;
const files = {
  "sibling.cjs": throwing,
  "sibling.mjs": throwing,
  "queue-microtask-after.cjs": `
process.on("exit", (c) => console.log("exit", c));
queueMicrotask(() => { throw new Error("qm-throw"); });
queueMicrotask(() => console.log("qm-after"));
process.nextTick(() => console.log("tick-after"));
setTimeout(() => console.log("timer-after"), 0);
`,
  "async-continuation.cjs": `
queueMicrotask(() => { throw new Error("qm-throw"); });
(async () => { await null; console.log("resumed"); })();
`,
  "in-timer.cjs": `
process.on("exit", (c) => console.log("exit", c));
setTimeout(() => {
  queueMicrotask(() => { throw new Error("qm-throw"); });
  Promise.resolve().then(() => console.log("p-after"));
}, 1);
setTimeout(() => console.log("timer2"), 100);
`,
  "monitor.cjs": `
process.on("uncaughtExceptionMonitor", (e, origin) => console.log("monitor", e.message, origin));
queueMicrotask(() => { throw new Error("qm-throw"); });
Promise.resolve().then(() => console.log("p-after"));
`,
  "listener-throws.mjs": `
process.on("uncaughtException", (e) => {
  throw new Error("from handler");
});
queueMicrotask(() => { throw new Error("orig"); });
Promise.resolve().then(() => console.log("p-after"));
`,
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);

function report(stderr) {
  return stderr
    .split(/\r?\n/)
    .filter((l) => l !== "" && !/^\s+at /.test(l) && !/^(Node\.js|oam) v\d/.test(l) && !l.startsWith("(Use `"))
    .map((l) => l.replace(/(?:file:\/\/\/)?(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.[cm]?js)/g, "$1"));
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
