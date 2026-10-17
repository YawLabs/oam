// A CommonJS module whose body throws while an ES module entry's import graph
// runs it (#437). node rejects the entry's evaluation with the thrown value and
// treats it as any other ES module entry throw: the uncaught ladder with origin
// 'unhandledRejection' (monitor, capture callback, listeners; 'uncaughtException'
// with a capture callback set), the fatal report when nothing takes it, exit 1
// -- or 7 when a listener throws -- and, when a listener takes it, no entry
// body but everything already queued. No ES module is imported ahead of the
// CommonJS one and the CommonJS modules ahead of it queue no tick: oam runs it
// while the graph loads, so those differ (docs/node-divergences.md #52, #54).
//
// Each scenario runs as a child; its stdout, status and stderr are printed
// line for line, blank lines kept. As in case 573: the hint's program name is
// made `node` on both sides, paths are cut to the file name, and of a stack
// only the frames in this case's own files are kept. node's own loader frame
// for a primitive (`node:internal/modules/run_main:123`, the three lines and
// the empty line before them, and the `Thrown at:` line naming it) names no
// source of the program's and is dropped: oam prints none
// (docs/node-divergences.md #54).
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-574-")));
const files = {
  "dep.cjs": "throw 42;\n",
  "deperr.cjs": 'const x = 1;\nthrow new Error("cjs boom");\n',
  "imp.mjs": 'import "./dep.cjs";\nconsole.log("entry body");\n',
  "imperr.mjs": 'import "./deperr.cjs";\nconsole.log("entry body");\n',
  // A listener takes it: the entry's body does not run; what was queued does.
  "lis.cjs": [
    'process.on("uncaughtExceptionMonitor", (e, o) => console.log("monitor", e instanceof Error ? e.message : e, o));',
    'process.on("uncaughtException", (e, o) => {',
    '  console.log("listener", e instanceof Error ? e.message : e, o);',
    '  process.nextTick(() => console.log("listener tick"));',
    '  Promise.resolve().then(() => console.log("listener microtask"));',
    '  setTimeout(() => console.log("listener timer"), 0);',
    "});",
    'process.on("exit", (c) => console.log("exit", c));',
    'setTimeout(() => console.log("lis timer"), 0);',
    "",
  ].join("\n"),
  "handled.mjs": 'import "./lis.cjs";\nimport "./dep.cjs";\nconsole.log("entry body");\n',
  "handlederr.mjs": 'import "./lis.cjs";\nimport "./deperr.cjs";\nconsole.log("entry body");\n',
  // A listener that throws: its error is the report, exit 7.
  "lis7.cjs":
    'process.on("uncaughtException", (e, o) => {\n  console.log("listener", e, o);\n  throw new Error("listener boom");\n});\n',
  "throws.mjs": 'import "./lis7.cjs";\nimport "./dep.cjs";\n',
  // The capture callback takes it ahead of the listeners.
  "cap.cjs": [
    'process.on("uncaughtExceptionMonitor", (e, o) => console.log("monitor", e, o));',
    'process.setUncaughtExceptionCaptureCallback((e) => console.log("capture", e));',
    'process.on("uncaughtException", (e, o) => console.log("listener", e, o));',
    'process.on("exit", (c) => console.log("exit", c));',
    "",
  ].join("\n"),
  "capture.mjs": 'import "./cap.cjs";\nimport "./dep.cjs";\n',
  // process.exitCode stands once a listener has taken it.
  "code.cjs":
    'process.exitCode = 5;\nprocess.on("uncaughtException", (e, o) => console.log("listener", e, o));\nprocess.on("exit", (c) => console.log("exit", c));\n',
  "code.mjs": 'import "./code.cjs";\nimport "./dep.cjs";\n',
  // Deeper in the graph: entry -> a.mjs -> dep.cjs.
  "a.mjs": 'import "./dep.cjs";\nconsole.log("a body");\n',
  "nested.mjs": 'import "./a.mjs";\nconsole.log("entry body");\n',
  "aerr.mjs": 'import "./deperr.cjs";\nconsole.log("a body");\n',
  "nestedlis.mjs": 'import "./lis.cjs";\nimport "./aerr.mjs";\nconsole.log("entry body");\n',
  // import() of such a graph rejects with the value itself.
  "dyn.mjs":
    'await import("./imp.mjs").then(() => console.log("resolved"), (e) => console.log("rejected", e));\nawait import("./deperr.cjs").then(() => console.log("resolved"), (e) => console.log("rejected", e.message));\n',
  // A Worker's entry: the parent's 'error' gets the value, the worker exits 1;
  // a listener in the worker takes it and the worker exits 0.
  "worker.mjs": [
    'import { Worker } from "node:worker_threads";',
    'for (const entry of ["imp.mjs", "imperr.mjs", "handled.mjs"]) {',
    "  await new Promise((resolve) => {",
    "    const w = new Worker(new URL(`./${entry}`, import.meta.url));",
    '    w.on("error", (e) => console.log(entry, "error", e instanceof Error ? e.message : e));',
    '    w.on("exit", (c) => {',
    '      console.log(entry, "worker exit", c);',
    "      resolve();",
    "    });",
    "  });",
    "}",
    "",
  ].join("\n"),
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);
const own = new Set(Object.keys(files));

function lines(stderr) {
  const out = [];
  const raw = stderr.split(/\r?\n/);
  for (let i = 0; i < raw.length; i++) {
    const line = raw[i];
    if (/^(Node\.js|oam) v\d/.test(line)) continue;
    // node's loader frame (#54): the frame's three lines, and the empty line
    // that opens it.
    if (/^node:internal\/modules\/run_main:\d+$/.test(line)) {
      if (out.length && out[out.length - 1] === "") out.pop();
      i += 2;
      continue;
    }
    const frame = /^\s+at (?:(.*?) \()?(.*?):(\d+):(\d+)\)?$/.exec(line);
    if (frame) {
      const base = frame[2].split(/[\\/]/).pop();
      if (own.has(base)) out.push(`    at ${frame[1] ? `${frame[1]} ` : ""}${base}:${frame[3]}:${frame[4]}`);
      continue;
    }
    out.push(
      line
        .replace(/(?:file:\/\/\/)?(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.(?:cjs|mjs))/g, "$1")
        .replace(/^\(Use `(node|oam) /, "(Use `node "),
    );
  }
  while (out.length && out[out.length - 1] === "") out.pop();
  return out;
}

function show(label, args) {
  const run = spawnSync(process.execPath, args, {
    cwd: dir,
    encoding: "utf8",
    env: { ...process.env, NODE_OPTIONS: "" },
  });
  console.log(`## ${label} (status ${run.status})`);
  for (const line of run.stdout.split(/\r?\n/)) {
    if (line !== "") console.log(`  out | ${line.replace(/(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.(?:cjs|mjs))/g, "$1")}`);
  }
  for (const line of lines(run.stderr)) console.log(`  err | ${JSON.stringify(line)}`);
}

try {
  for (const name of ["imp.mjs", "imperr.mjs", "nested.mjs"]) {
    show(`${name}, no flag`, [name]);
    show(`${name}, --trace-uncaught`, ["--trace-uncaught", name]);
  }
  for (const name of [
    "handled.mjs",
    "handlederr.mjs",
    "throws.mjs",
    "capture.mjs",
    "code.mjs",
    "nestedlis.mjs",
    "dyn.mjs",
    "worker.mjs",
  ]) {
    show(name, [name]);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
