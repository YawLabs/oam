// node's compile cache end to end: a program that enables it with
// module.enableCompileCache(dir) and then loads CJS (require) and ESM
// (import()) code writes entries into the directory it named, and its warm
// second run prints exactly what the cold first run did. Entries that are
// corrupted on disk are rejected and recompiled, never run; the output is
// still the same. A program that exits through process.exit() persists its
// static ESM graph's entries (NODE_COMPILE_CACHE, then exit mid-graph), as
// node does. The count of entries is runtime-specific, so only "some" vs
// "none" is shown.
//
// Regression guard: oam had no enableCompileCache (TypeError), ignored
// NODE_COMPILE_CACHE, and lost the pending ESM entries on process.exit().
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "cc439-"));
const put = (name, text) => fs.writeFileSync(path.join(scratch, name), text);
put("lib.cjs", `
"use strict";
function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
class Box { constructor(v) { this.v = v; } twice() { return this.v * 2; } }
module.exports = { fib, Box, kind: typeof require };
`);
put("lib.mjs", `
export const tag = "esm";
export function sum(...xs) { return xs.reduce((a, b) => a + b, 0); }
export default class Point { constructor(x, y) { this.x = x; this.y = y; } toString() { return \`(\${this.x},\${this.y})\`; } }
`);
put("main.cjs", `
const m = require("node:module");
const r = m.enableCompileCache(process.argv[2]);
console.log("status", r.status, typeof r.directory);
const lib = require("./lib.cjs");
console.log("cjs", lib.fib(15), new lib.Box(21).twice(), lib.kind);
import("./lib.mjs").then((esm) => {
  console.log("esm", esm.tag, esm.sum(1, 2, 3), String(new esm.default(3, 4)));
  console.log("flush", m.flushCompileCache());
});
`);
put("exit.mjs", `
import Point, { sum } from "./lib.mjs";
console.log("exit", sum(4, 5), String(new Point(1, 2)));
process.exit(0);
`);

const run = (file, args = [], env = {}) => {
  // OAM_CACHE_DIR (a harness's isolation) stays: the directory this case
  // names outranks it.
  const e = { ...process.env, ...env };
  delete e.OAM_CODE_CACHE;
  delete e.NODE_DISABLE_COMPILE_CACHE;
  if (!("NODE_COMPILE_CACHE" in env)) delete e.NODE_COMPILE_CACHE;
  return execFileSync(process.execPath, [path.join(scratch, file), ...args], { env: e, cwd: scratch, encoding: "utf8" });
};
const files = (dir) => {
  const out = [];
  const walk = (d) => {
    if (!fs.existsSync(d)) return;
    for (const ent of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, ent.name);
      if (ent.isDirectory()) walk(p);
      else if (!ent.name.startsWith(".")) out.push(p);
    }
  };
  walk(dir);
  return out;
};
const some = (dir) => (files(dir).length > 0 ? "some" : "none");

const cache = path.join(scratch, "cache");
console.log("before", some(cache));
const cold = run("main.cjs", [cache]);
process.stdout.write(cold);
console.log("after cold", some(cache));
const warm = run("main.cjs", [cache]);
console.log("warm identical", warm === cold);

for (const f of files(cache)) fs.writeFileSync(f, Buffer.alloc(64, 0x5a));
const corrupt = run("main.cjs", [cache]);
console.log("corrupt identical", corrupt === cold);
const again = run("main.cjs", [cache]);
console.log("rewarmed identical", again === cold);

const exitCache = path.join(scratch, "exit-cache");
process.stdout.write(run("exit.mjs", [], { NODE_COMPILE_CACHE: exitCache }));
console.log("after exit", some(exitCache));
const exitWarm = run("exit.mjs", [], { NODE_COMPILE_CACHE: exitCache });
console.log("exit warm", exitWarm.trim());

fs.rmSync(scratch, { recursive: true, force: true });
