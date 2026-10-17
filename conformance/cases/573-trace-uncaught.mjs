// --trace-uncaught (#399 item 6). Without it, a fatal report of a thrown
// primitive ends with the hint that names the flag; with it -- on the
// command line, in NODE_OPTIONS, or in a Worker's own execArgv -- every
// fatal report ends with "Thrown at:" and the stack V8 captured at the
// throw, in place of the hint.
//
// Each scenario runs as a child; its stdout, status and stderr are printed
// line for line, blank lines kept. The hint's program name is made `node`
// on both sides (as in case 406), paths are cut to the file name, and of a
// stack only the frames in this case's own files are kept: the frames past
// them are each runtime's own (node:internal/... in node, oam:... in oam).
// A primitive thrown from an ES module entry or a queueMicrotask callback
// is docs/node-divergences.md #54, and stays out.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-573-")));
const files = {
  "top.cjs": "throw 42;\n",
  "nested.cjs": 'function inner() {\n  throw "from inner";\n}\nfunction outer() {\n  inner();\n}\nouter();\n',
  "timer.cjs": "setTimeout(function later() {\n  throw 42;\n}, 1);\n",
  "tick.cjs": "process.nextTick(() => {\n  throw null;\n});\n",
  "rethrow.cjs": "try {\n  throw 1;\n} catch (e) {\n  throw e;\n}\n",
  "object.cjs": "function make() {\n  throw { a: 1 };\n}\nmake();\n",
  "error.cjs": 'function boom() {\n  throw new Error("e");\n}\nboom();\n',
  // A sourceURL names the report's source frame, but not the Thrown at
  // lines, which keep the script's own name; an eval's is empty.
  "source-url.cjs": "function inner() {\n  throw 5;\n}\ninner();\n//# sourceURL=renamed.js\n",
  "eval.cjs": 'eval("throw 6\\n//# sourceURL=evaluated.js");\n',
  "listener.cjs":
    'process.on("uncaughtException", function onUncaught() {\n  throw 43;\n});\nthrow new Error("x");\n',
  "exec-argv.cjs": "console.log(JSON.stringify(process.execArgv));\nthrow 42;\n",
  "worker-main.cjs": [
    'const { Worker } = require("node:worker_threads");',
    'const w = new Worker(require("node:path").join(__dirname, "worker.cjs"), { execArgv: ["--trace-uncaught"] });',
    'w.on("message", (m) => console.log("worker execArgv", JSON.stringify(m)));',
    'w.on("error", (e) => console.log("worker error", e));',
    'w.on("exit", (c) => console.log("worker exit", c));',
    "",
  ].join("\n"),
  "worker.cjs": 'require("node:worker_threads").parentPort.postMessage(process.execArgv);\nthrow 42;\n',
};
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);
const own = new Set(Object.keys(files));

function lines(stderr) {
  const out = [];
  for (const line of stderr.split(/\r?\n/)) {
    if (/^(Node\.js|oam) v\d/.test(line)) continue;
    const frame = /^\s+at (?:(.*?) \()?(.*?):(\d+):(\d+)\)?$/.exec(line);
    if (frame) {
      const base = frame[2].split(/[\\/]/).pop();
      // An eval frame's script is empty ("[eval] (:1:1)"): kept as is.
      if (frame[2] === "") out.push(line.trim().replace(/^at /, "    at "));
      else if (own.has(base)) out.push(`    at ${frame[1] ? `${frame[1]} ` : ""}${base}:${frame[3]}:${frame[4]}`);
      continue;
    }
    out.push(
      line
        .replace(/(?:file:\/\/\/)?(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.cjs)/g, "$1")
        .replace(/^\(Use `(node|oam) /, "(Use `node "),
    );
  }
  while (out.length && out[out.length - 1] === "") out.pop();
  return out;
}

function show(label, args, env) {
  const run = spawnSync(process.execPath, args, {
    cwd: dir,
    encoding: "utf8",
    env: { ...process.env, NODE_OPTIONS: "", ...env },
  });
  console.log(`## ${label} (status ${run.status})`);
  for (const line of run.stdout.split(/\r?\n/)) if (line !== "") console.log(`  out | ${line}`);
  for (const line of lines(run.stderr)) console.log(`  err | ${JSON.stringify(line)}`);
}

try {
  for (const name of ["top.cjs", "nested.cjs", "object.cjs"]) show(`${name}, no flag`, [name]);
  for (const name of [
    "top.cjs",
    "nested.cjs",
    "timer.cjs",
    "tick.cjs",
    "rethrow.cjs",
    "object.cjs",
    "error.cjs",
    "source-url.cjs",
    "eval.cjs",
    "listener.cjs",
  ]) {
    show(`${name}, --trace-uncaught`, ["--trace-uncaught", name]);
  }
  // NODE_OPTIONS turns it on too, but is not execArgv.
  show("exec-argv.cjs, NODE_OPTIONS", ["exec-argv.cjs"], { NODE_OPTIONS: "--trace-uncaught" });
  show("exec-argv.cjs, --trace-uncaught", ["--trace-uncaught", "exec-argv.cjs"]);
  // A Worker's own execArgv is accepted, and its process.execArgv lists it;
  // the parent's 'error' gets the value itself.
  show("worker-main.cjs", ["worker-main.cjs"]);
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
