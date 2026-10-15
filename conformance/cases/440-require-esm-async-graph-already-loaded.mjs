// require() of an ES module whose graph has a top-level await, when an
// `import` statement has already loaded it, is node 22's
// ERR_REQUIRE_ASYNC_MODULE both while the module is part-way through its
// await and after it has finished. V8 reports a module suspended in its
// await as evaluated, and node's getNamespaceSync tests the graph for a
// top-level await before it reads the namespace, so neither require()
// hands out a namespace -- the mid-await one would have bindings still
// uninitialized. A module that threw after its await is rethrown as the
// same error instead.
//
// Regression guard: the first oam require(esm) returned the namespace of
// an already-evaluated module without the test, so the mid-await require()
// threw a raw ReferenceError ("default is not defined") from the
// uninitialized binding and the later one returned the namespace.
//
// Sequenced by events, not timers: early.mjs evaluates first (import
// order) and queues a timer; tla.mjs then parks on a promise only that
// timer resolves, after its mid-await require(). The entry's body runs once
// tla.mjs has finished. The entry runs on the SAME runtime
// (process.execPath) from a pid-scoped fixture directory; stdout carries
// no paths.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-async-loaded-${process.pid}`);
const prelude =
  "import { createRequire } from 'node:module';\n" +
  "const require = createRequire(import.meta.url);\n" +
  "const attempt = (label, specifier) => {\n" +
  "  try {\n" +
  "    const ns = require(specifier);\n" +
  "    console.log(label, 'returned', Object.keys(ns));\n" +
  "  } catch (e) {\n" +
  "    console.log(label, e.code, e.name, e.message.split('\\n')[0]);\n" +
  "    return e;\n" +
  "  }\n" +
  "};\n";
const files = {
  "early.mjs":
    prelude +
    "setTimeout(() => {\n" +
    "  attempt('mid-await', './tla.mjs');\n" +
    "  globalThis.release();\n" +
    "}, 0);\n",
  "tla.mjs":
    "console.log('tla before await');\n" +
    "await new Promise((resolve) => { globalThis.release = resolve; });\n" +
    "console.log('tla after await');\n" +
    "export const v = 1;\n" +
    "export default 'd';\n",
  "throws.mjs": "await 0;\nthrow new Error('thrown after the await');\n",
  "entry.mjs":
    prelude +
    "import './early.mjs';\n" +
    "import './tla.mjs';\n" +
    "attempt('after it finished', './tla.mjs');\n" +
    "attempt('after it finished, again', './tla.mjs');\n",
  "errored.cjs":
    "import('./throws.mjs').catch((first) => {\n" +
    "  console.log('import()', first.message);\n" +
    "  try { require('./throws.mjs'); } catch (e) {\n" +
    "    console.log('errored after its await', e.code, e.message, e === first);\n" +
    "  }\n" +
    "});\n",
};

try {
  mkdirSync(dir, { recursive: true });
  for (const [name, source] of Object.entries(files)) {
    writeFileSync(path.join(dir, name), source);
  }
  for (const entry of ["entry.mjs", "errored.cjs"]) {
    const r = spawnSync(process.execPath, [path.join(dir, entry)], { encoding: "utf8" });
    process.stdout.write(r.stdout);
    console.log(`${entry} exit ${r.status}`);
  }
} finally {
  rmSync(dir, { recursive: true, force: true });
}
