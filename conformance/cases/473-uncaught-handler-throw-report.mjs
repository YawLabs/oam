// The fatal report, and what runs before it, on paths around a throwing
// 'uncaughtException' listener and the -e entry (#399 items 2, 5, 6, 10,
// 11).
//
// - A listener that throws is fatal, exit 7, and the report's source frame
//   is the listener's own `throw` -- not a line of the runtime's dispatcher
//   -- from a CommonJS entry, a timer and an I/O callback.
// - A listener that handles one throw, queues a tick and a promise job, and
//   throws on the next: what it queued on the fatal call never runs --
//   from the entry and its ticks, from timers (far enough apart that the
//   second is never due in the first's timers phase) and from I/O.
// - `throw 42` from a CommonJS entry: the source frame, then the value,
//   and the report's blank lines as node's, for each kind of thrown value.
//   (node then names `--trace-uncaught`, which oam does not have; the hint
//   is taken out as in case 406. A primitive from an ES module entry is
//   docs/node-divergences.md #54.)
// - `-e`: the 'exit' listeners run, then the report is printed.
// - An ES module entry rejected in one V8 task's drain is reported before
//   any other task of the batch runs.
//
// Each scenario runs as a child; its stdout and status are printed, and its
// stderr as the report's source frame and message lines (no stack, no
// runtime banner, paths cut to the file name).
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-473-"));
const handledThenFatal = (trigger) => `
let calls = 0;
process.on("uncaughtException", (e) => {
  calls++;
  console.log("handler " + e.message);
  process.nextTick(() => console.log("tick from " + e.message));
  Promise.resolve().then(() => console.log("promise from " + e.message));
  if (calls === 2) throw new Error("handler threw");
});
${trigger}
`;
const files = {
  "listener-throws.cjs": `
process.on("uncaughtException", (e) => {
  throw new Error("from handler");
});
throw new Error("orig");
`,
  "listener-throws-timer.cjs": `
process.on("uncaughtException", (e) => {
  throw new Error("from handler");
});
setTimeout(() => { throw new Error("orig"); }, 1);
`,
  "handled-then-fatal.cjs": handledThenFatal(`
process.nextTick(() => { throw new Error("B"); });
throw new Error("A");
`),
  "handled-then-fatal-timer.cjs": handledThenFatal(`
setTimeout(() => { throw new Error("A"); }, 1);
setTimeout(() => { throw new Error("B"); }, 50);
`),
  "handled-then-fatal-io.cjs": handledThenFatal(`
require("fs").stat(__filename, () => { throw new Error("A"); });
setTimeout(() => require("fs").stat(__filename, () => { throw new Error("B"); }), 50);
`),
  "throw-primitive.cjs": `
throw 42;
`,
  "task-batch.mjs": `
const i32 = new Int32Array(new SharedArrayBuffer(16));
const a = Atomics.waitAsync(i32, 0, 0, 30).value;
const b = Atomics.waitAsync(i32, 1, 0, 30).value;
b.then(() => console.log("b continuation"));
setTimeout(() => console.log("timer"), 3000);
await a;
throw new Error("entry");
`,
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);

function report(stderr) {
  return stderr
    .split(/\r?\n/)
    .filter((l) => l !== "" && !/^\s+at /.test(l) && !/^(Node\.js|oam) v\d/.test(l) && !l.startsWith("(Use `"))
    .map((l) => l.replace(/(?:file:\/\/\/)?(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.[cm]?js)/g, "$1"));
}

function show(label, args) {
  const run = spawnSync(process.execPath, args, { cwd: dir, encoding: "utf8" });
  console.log(`## ${label} (status ${run.status})`);
  for (const line of run.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
  for (const line of report(run.stderr)) console.log(`  err | ${line}`);
}

try {
  for (const file of Object.keys(files)) show(file, [file]);
  // -e: the eval's own source frame is named differently by each runtime
  // ("[eval]" in node), so only the 'exit' listener's line and the message
  // line are compared, in the order printed.
  // A thrown value that is not an Error, from CommonJS, line for line with
  // the blank lines kept: node opens the report with an empty line, puts the
  // value right under the caret, and prints a primitive as String() does
  // (a Symbol as nothing); an object is inspected. A native Error keeps the
  // empty line after the caret. Only the stack, the banner and node's
  // `--trace-uncaught` hint (which follows a primitive) are taken out.
  const values = {
    "number.cjs": "throw 42;",
    "string.cjs": 'throw "a string";',
    "bigint.cjs": "throw 5n;",
    "null.cjs": "throw null;",
    "symbol.cjs": 'throw Symbol("y");',
    "object.cjs": "throw { a: 1 };",
    "error-prototype.cjs": "throw Object.create(Error.prototype);",
    "error.cjs": 'throw new Error("e");',
    "timer-number.cjs": "setTimeout(() => { throw 42; }, 1);",
    "tick-string.cjs": 'process.nextTick(() => { throw "from a tick"; });',
    "listener-throws-number.cjs": 'process.on("uncaughtException", () => { throw 43; });\nthrow new Error("x");',
  };
  for (const [name, text] of Object.entries(values)) {
    fs.writeFileSync(path.join(dir, name), `${text}\n`);
    const run = spawnSync(process.execPath, [name], { cwd: dir, encoding: "utf8" });
    console.log(`## ${name} lines (status ${run.status})`);
    const lines = run.stderr
      .split(/\r?\n/)
      .filter((l) => !/^\s+at /.test(l) && !/^(Node\.js|oam) v\d/.test(l) && !l.startsWith("(Use `"))
      .map((l) => l.replace(/(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.cjs)/g, "$1"));
    for (const line of lines) console.log(`  err | ${JSON.stringify(line)}`);
  }
  const evalRun = spawnSync(
    process.execPath,
    ["-e", 'process.on("exit", (c) => console.error("exit listener", c)); throw new Error("eval fatal");'],
    { cwd: dir, encoding: "utf8" },
  );
  console.log(`## -e (status ${evalRun.status})`);
  for (const line of evalRun.stderr.split(/\r?\n/)) {
    if (line.startsWith("exit listener") || line.startsWith("Error: eval fatal")) console.log(`  err | ${line}`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
