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
// node links the whole graph before it evaluates any of it, so when the graph
// also has a link or syntax error -- or an import that does not resolve --
// that error is the report, exit 1, and no listener sees the CommonJS value.
// oam has run the CommonJS modules ahead of the failure by then (#52), so of
// those scenarios only the status, the listener's lines and the SyntaxError's
// own line are compared, and the listener registers for 'uncaughtException'
// only. A CommonJS module after the throwing one is never run, by either, and
// a named import of it -- or of the throwing one, directly or through
// `export *` -- that its exports have is no link error.
//
// A capture callback that throws on the value: node's loader called it from
// an async function, so the throw rejects that function's promise and node's
// rejection pass offers it to the monitor and the callback again, origin
// 'unhandledRejection' -- the same for an ES module entry's own throw.
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
    'for (const entry of ["imp.mjs", "imperr.mjs", "handled.mjs", "link.mjs"]) {',
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
  // The graph fails to link or load as well: that failure is the report.
  "uelis.cjs": 'process.on("uncaughtException", (e, o) => console.log("listener", e, o));\n',
  "x.mjs": "export const yes = 1;\n",
  "bad.mjs": "export const = 1;\n",
  "later.cjs": 'console.log("later body");\nexports.later = 1;\n',
  "link.mjs": 'import "./dep.cjs";\nimport { nope } from "./x.mjs";\n',
  "linklis.mjs": 'import "./uelis.cjs";\nimport "./dep.cjs";\nimport { nope } from "./x.mjs";\n',
  "syntax.mjs": 'import "./dep.cjs";\nimport "./bad.mjs";\n',
  "syntaxlis.mjs": 'import "./uelis.cjs";\nimport "./dep.cjs";\nimport "./bad.mjs";\n',
  "nestedsyntax.mjs": 'import "./uelis.cjs";\nimport "./a.mjs";\nimport "./c.mjs";\n',
  "c.mjs": 'import "./bad.mjs";\n',
  "m.mjs": 'import "./missing.mjs";\n',
  "missinglis.mjs": 'import "./uelis.cjs";\nimport "./dep.cjs";\nimport "./m.mjs";\n',
  "nestedlink.mjs": 'import "./uelis.cjs";\nimport "./dep.cjs";\nimport "./n.mjs";\n',
  "n.mjs": 'import { nope } from "./x.mjs";\n',
  "unres.mjs": 'import "./dep.cjs";\nimport "./m.mjs";\n',
  // A name asked through `export *` of the module that throws, or of one
  // after it, links: the throw is the report.
  "t.cjs": "exports.x = 1;\nthrow 43;\n",
  "r.mjs": 'export * from "./t.cjs";\n',
  "r2.mjs": 'export * from "./r.mjs";\n',
  "cyc.mjs": 'export * from "./cyc2.mjs";\nexport * from "./t.cjs";\n',
  "cyc2.mjs": 'export * from "./cyc.mjs";\n',
  "star.mjs": 'import "./uelis.cjs";\nimport { x } from "./r.mjs";\nconsole.log("entry body", x);\n',
  "starnolis.mjs": 'import { x } from "./r.mjs";\nconsole.log("entry body", x);\n',
  "starchain.mjs": 'import "./uelis.cjs";\nimport { x } from "./r2.mjs";\nconsole.log("entry body", x);\n',
  "starcycle.mjs": 'import "./uelis.cjs";\nimport { x } from "./cyc2.mjs";\nconsole.log("entry body", x);\n',
  "starlater.mjs":
    'import "./uelis.cjs";\nimport "./dep.cjs";\nimport { later } from "./rl.mjs";\nconsole.log("entry body", later);\n',
  "rl.mjs": 'export * from "./later.cjs";\n',
  // A capture callback that throws: node's loader called it from an async
  // function, so its throw is an unhandled rejection -- the monitor and the
  // callback see it once more, origin 'unhandledRejection'. Thrown once,
  // the second call takes it; thrown each time, the report, exit 7.
  "capthrow.cjs": [
    "let calls = 0;",
    'process.on("uncaughtExceptionMonitor", (e, o) => console.log("monitor", e instanceof Error ? e.message : e, o));',
    "process.setUncaughtExceptionCaptureCallback((e) => {",
    '  console.log("capture", e instanceof Error ? e.message : e);',
    '  if (calls++ === 0 || process.env.CAPTURE_THROWS) throw new Error("capture boom");',
    "});",
    'process.on("exit", (c) => console.log("exit", c));',
    "",
  ].join("\n"),
  "capthrow.mjs": 'import "./capthrow.cjs";\nimport "./dep.cjs";\n',
  "capthrowown.mjs": 'import "./capthrow.cjs";\nthrow 44;\n',
  // It links: the throw is the report, and later.cjs never runs.
  "laterok.mjs":
    'import "./uelis.cjs";\nimport "./dep.cjs";\nimport { yes } from "./x.mjs";\nimport { later } from "./later.cjs";\nconsole.log("entry body", yes, later);\n',
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

// A hung scenario child is a child-level failure: killed here (25s) and
// printed with its signal, rather than blocking until the harness's
// CASE_CEILING kills this case and truncates one side's whole output.
const CHILD_MS = 25_000;

function show(label, args) {
  const run = spawnSync(process.execPath, args, {
    cwd: dir,
    encoding: "utf8",
    timeout: CHILD_MS,
    env: { ...process.env, NODE_OPTIONS: "" },
  });
  console.log(`## ${label} (status ${run.status}${run.signal ? ` signal ${run.signal}` : ""})`);
  for (const line of run.stdout.split(/\r?\n/)) {
    if (line !== "") console.log(`  out | ${line.replace(/(?:[A-Za-z]:)?[^\s:]*[\\/]([^\\/\s:]+\.(?:cjs|mjs))/g, "$1")}`);
  }
  for (const line of lines(run.stderr)) console.log(`  err | ${JSON.stringify(line)}`);
}

// For a graph that fails to link or load: the status, stdout, and of stderr
// only the SyntaxError's own line and whether the CommonJS value is what it
// reports.
function showFailure(label, args, env = {}) {
  const run = spawnSync(process.execPath, args, {
    cwd: dir,
    encoding: "utf8",
    timeout: CHILD_MS,
    env: { ...process.env, NODE_OPTIONS: "", ...env },
  });
  console.log(`## ${label} (status ${run.status}${run.signal ? ` signal ${run.signal}` : ""})`);
  for (const line of run.stdout.split(/\r?\n/)) {
    if (line !== "") console.log(`  out | ${line}`);
  }
  for (const line of run.stderr.split(/\r?\n/)) {
    const error = /(SyntaxError: .*)$/.exec(line);
    if (error) console.log(`  err | ${JSON.stringify(error[1])}`);
  }
  console.log(`  reports the value: ${/^(?:.*Uncaught )?4[234]$/m.test(run.stderr)}`);
}

try {
  for (const name of [
    "link.mjs",
    "linklis.mjs",
    "nestedlink.mjs",
    "syntax.mjs",
    "syntaxlis.mjs",
    "nestedsyntax.mjs",
    "unres.mjs",
    "missinglis.mjs",
  ]) {
    showFailure(name, [name]);
  }
  for (const name of ["laterok.mjs", "star.mjs", "starnolis.mjs", "starchain.mjs", "starcycle.mjs", "starlater.mjs"]) {
    show(name, [name]);
  }
  for (const name of ["capthrow.mjs", "capthrowown.mjs"]) {
    show(name, [name]);
    // The callback's frames name it differently (node's captureFn, oam's
    // own), so of this report only the status and stdout.
    showFailure(`${name}, thrown each time`, [name], { CAPTURE_THROWS: "1" });
  }
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
