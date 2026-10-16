// On which microtask turn import() evaluates the module and settles (#397).
// node's loader is a chain of promise jobs -- resolve, load and translate,
// link each module's requests, instantiate the graph, evaluate, hand back
// the namespace -- so a chain of promise jobs the importer started just
// before the call counts the turns: a module with no imports of its own
// evaluates on turn 8 and import() settles on turn 14; one already loaded
// settles on turn 9; each level of static imports below the module adds
// turns before it evaluates; a resolution failure rejects on turn 5, a
// load failure on turn 6, a missing export on turn 23; a top-level await
// settles import() seven turns after the module's last await; a namespace
// that exports `then` is a thenable, one turn more. The same from a
// CommonJS importer.
//
// Regression guard: oam evaluated the module inside the import hook, on
// turn 0, and settled import() at once.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-481-"));
const at = (name) => `__log.push("${name}@" + __n);\n`;
const files = {
  "counter.cjs": `
globalThis.__n = 0;
globalThis.__log = [];
function step() { __n++; if (__n < 60) Promise.resolve().then(step); }
module.exports = { start() { Promise.resolve().then(step); } };
`,
  "leaf.mjs": at("leaf"),
  // When a CommonJS module RUNS is left out: oam runs it as it loads it,
  // node as the graph evaluates (docs/node-divergences.md). When import()
  // settles is in.
  "leaf.cjs": "module.exports = { leaf: true };\n",
  "data.json": '{ "a": 1 }\n',
  "loaded.mjs": "export const loaded = true;\n",
  "required.cjs": "module.exports = {};\n",
  "tla.mjs": `${at("tla-start")}await null;\n${at("tla-after-1")}await null;\n${at("tla-end")}`,
  "throws.mjs": `${at("throws")}throw new Error("boom");\n`,
  "thenable.mjs": `${at("thenable")}export function then(resolve) { __log.push("then@" + __n); resolve(42); }\n`,
  "self.mjs": `${at("self")}import("./self.mjs").then(() => __log.push("self-settled@" + __n));\n`,
  "c2.mjs": `import "./c3.mjs";\n${at("c2")}`,
  "c3.mjs": at("c3"),
  "chain2.mjs": `import "./leaf2.mjs";\n${at("chain2")}`,
  "leaf2.mjs": "",
  "chain3.mjs": `import "./c2.mjs";\n${at("chain3")}`,
  "chain4.mjs": `import "./chain3.mjs";\n${at("chain4")}`,
  "d-b.mjs": 'import "./d-d.mjs";\n',
  "d-c.mjs": 'import "./d-d.mjs";\n',
  "d-d.mjs": "",
  "diamond.mjs": `import "./d-b.mjs";\nimport "./d-c.mjs";\n${at("diamond")}`,
  "s1.mjs": 'import "./s2.mjs";\n',
  "s2.mjs": 'import "./s3.mjs";\n',
  "s3.mjs": "",
  "on-static.mjs": `import "./s1.mjs";\n${at("on-static")}`,
  "on-loaded.mjs": `import "./loaded.mjs";\n${at("on-loaded")}`,
  "dep.cjs": "",
  "mixed.mjs": `import "./dep.cjs";\nimport "node:path";\n${at("mixed")}`,
  "syntax.mjs": `${at("syntax")}let let = ;\n`,
  "no-export.mjs": `import { missing } from "./leaf2.mjs";\n${at("no-export")}`,
};
const body = `
const which = process.argv[2];
require("./required.cjs");
const { start } = require("./counter.cjs");
start();
const call = which === "./data.json" ? import(which, { with: { type: "json" } })
  : which === "./bad-type" ? import("./leaf2.mjs", { with: { type: "css" } })
  : import(which);
call.then(() => __log.push("settled@" + __n), () => __log.push("rejected@" + __n));
if (process.argv[3] === "twice") import(which).then(() => __log.push("settled-again@" + __n));
setTimeout(() => console.log(__log.join(" ")), 20);
`;
files["importer.mjs"] = `import "./s1.mjs";
import { loaded } from "./loaded.mjs";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
${body}`;
files["importer.cjs"] = body;
for (const [name, text] of Object.entries(files)) fs.writeFileSync(path.join(dir, name), text);
try {
  const shapes = [
    ["ES module", "./leaf.mjs"],
    ["CommonJS", "./leaf.cjs"],
    ["JSON", "./data.json"],
    ["builtin", "node:os"],
    ["CommonJS required before", "./required.cjs"],
    ["already loaded", "./loaded.mjs"],
    ["imported twice at once", "./leaf.mjs", "twice"],
    ["top-level await", "./tla.mjs"],
    ["top-level throw", "./throws.mjs"],
    ["namespace exporting then", "./thenable.mjs"],
    ["importing itself", "./self.mjs"],
    ["two levels", "./chain2.mjs"],
    ["three levels", "./chain3.mjs"],
    ["four levels", "./chain4.mjs"],
    ["diamond", "./diamond.mjs"],
    ["on a loaded graph", "./on-static.mjs"],
    ["on a loaded module", "./on-loaded.mjs"],
    ["on CommonJS and a builtin", "./mixed.mjs"],
    ["unresolvable", "./missing.mjs"],
    ["syntax error", "./syntax.mjs"],
    ["missing export", "./no-export.mjs"],
    ["unsupported type attribute", "./bad-type"],
  ];
  for (const importer of ["importer.mjs", "importer.cjs"]) {
    for (const [label, ...args] of shapes) {
      const run = spawnSync(process.execPath, [importer, ...args], { cwd: dir, encoding: "utf8" });
      console.log(`${importer}, ${label}: ${run.stdout.trim()}${run.status === 0 ? "" : ` (status ${run.status})`}`);
    }
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
