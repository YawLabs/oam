// What a throw does on the way out (#399 items 1, 7 and 9).
//
// - A promise job an 'exit' listener queues runs, and its throw is an
//   uncaught exception like any other: reported, exit 1, or taken by an
//   'uncaughtException' listener.
// - A throw from an 'exit' listener goes to the 'uncaughtException'
//   listeners too. Reported or not, the emit ends there: later listeners,
//   and the promise jobs the listeners queued, do not run. The code is the
//   process.exitCode already set (process.exit(3) sets it), else 1.
// - Past a fatal throw node emits 'exit' inside a try/catch that drops what
//   a listener throws: the original report is the only one.
//
// Each scenario runs as a child; its stdout and status are printed, and its
// stderr as the report's source frame and message lines (no stack, no
// runtime banner, paths cut to the file name).
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-472-"));
const files = {
  "microtask-throw-in-exit.cjs": `
process.on("exit", () => queueMicrotask(() => { throw new Error("from a promise job of 'exit'"); }));
`,
  "microtask-throw-in-exit-handled.cjs": `
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
process.on("exit", () => queueMicrotask(() => { throw new Error("from a promise job of 'exit'"); }));
`,
  "microtask-throw-in-exit-keeps-code.cjs": `
process.exitCode = 5;
process.on("exit", () => queueMicrotask(() => { throw new Error("from a promise job of 'exit'"); }));
`,
  "listener-throws.cjs": `
process.on("exit", () => {
  Promise.resolve().then(() => console.log("promise job ran"));
  throw new Error("in exit");
});
process.on("exit", () => console.log("second listener"));
`,
  "listener-throws-keeps-code.cjs": `
process.on("exit", () => { process.exitCode = 4; throw new Error("in exit"); });
`,
  "listener-throws-after-exit-call.cjs": `
process.on("exit", () => { throw new Error("in exit"); });
process.exit(3);
`,
  "listener-throws-handled.cjs": `
process.on("exit", () => {
  process.exitCode = 4;
  Promise.resolve().then(() => console.log("promise job ran"));
  throw new Error("in exit");
});
process.on("uncaughtException", (e) => {
  console.log("uncaughtException", e.message);
  queueMicrotask(() => console.log("listener's promise job ran"));
});
process.on("exit", () => console.log("second listener"));
`,
  "listener-throws-handled-no-code.cjs": `
process.on("exit", () => { throw new Error("in exit"); });
process.on("uncaughtException", (e) => console.log("uncaughtException", e.message));
`,
  "fatal-then-listener-throws.cjs": `
process.on("exit", (c) => { console.log("exit", c); throw new Error("exit listener"); });
throw new Error("orig");
`,
  "fatal-then-listener-throws.mjs": `
process.on("exit", (c) => { console.log("exit", c); throw new Error("exit listener"); });
throw new Error("orig");
`,
  "timer-fatal-then-listener-throws.cjs": `
process.on("exit", (c) => { console.log("exit", c); throw new Error("exit listener"); });
setTimeout(() => { throw new Error("orig"); }, 1);
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
  // A promise an 'exit' listener rejects and leaves unhandled is fatal too.
  // Only the status and stdout: oam reports an unhandled rejection in its
  // own `error[OAM-RT0004]` format, not as node's fatal report.
  fs.writeFileSync(path.join(dir, "rejection-in-exit.cjs"), `
process.on("exit", () => Promise.reject(new Error("rejected in exit")));
`);
  const rejection = spawnSync(process.execPath, ["rejection-in-exit.cjs"], { cwd: dir, encoding: "utf8" });
  console.log(`## rejection-in-exit.cjs (status ${rejection.status})`);
  for (const line of rejection.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
