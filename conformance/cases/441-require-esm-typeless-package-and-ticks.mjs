// Two things node 22's require() of an ES module does that oam got wrong:
//
// 1. A package in node_modules with no "type" whose main file is written
//    with module syntax is detected as an ES module and loaded as one: the
//    required namespace has the module's exports (with the __esModule
//    facade when there is a default export), and a later import() of the
//    package gets the same module. oam takes a typeless file in
//    node_modules for CommonJS; the require() found the module syntax, but
//    the graph load then ran the file as CommonJS against the placeholder
//    require.cache entry the require() had made, so the namespace was
//    `{ __esModule: true, default: {} }` and import() saw `{ default: {} }`.
//
// 2. require() runs no ticks and no promise jobs before it returns, even
//    when a CommonJS module in the required graph calls import() (oam loads
//    that import's graph inside the require). The requiring code's
//    process.nextTick and queueMicrotask callbacks run after it, in node's
//    order. oam ran both inside the require(), before it returned.
//
// The entry runs on the SAME runtime (process.execPath) from a pid-scoped
// fixture directory; stdout carries no paths.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-typeless-${process.pid}`);
const files = {
  "node_modules/named/package.json": '{ "name": "named" }',
  "node_modules/named/index.js": "export const named = 1;\n",
  "node_modules/withdefault/package.json": '{ "name": "withdefault" }',
  "node_modules/withdefault/index.js":
    "export default function hello() { return 'hi'; }\nexport const z = 2;\n",
  "packages.cjs": `
const named = require('named');
console.log('named', Object.keys(named), named.named);
const withDefault = require('withdefault');
console.log('withdefault', Object.keys(withDefault), typeof withDefault.default,
  withDefault.default(), withDefault.z);
import('named').then((ns) => console.log('import() of it: same namespace', ns === named));
`,
  // The import() target, silent: when its graph evaluates differs between
  // the runtimes (oam loads it inside the import() call), which is not what
  // this case pins.
  "x.mjs": "import './d.cjs';\nexport const x = 1;\n",
  "d.cjs": "module.exports = 'd';\n",
  "c.cjs": "console.log('c ran');\nimport('./x.mjs').then(() => console.log('x imported'));\n",
  "r.mjs": "import './c.cjs';\nconsole.log('r evaluated');\n",
  "ticks.cjs": `
queueMicrotask(() => console.log('main microtask'));
process.nextTick(() => console.log('main tick'));
require('./r.mjs');
console.log('require returned');
`,
};

try {
  for (const [name, source] of Object.entries(files)) {
    const file = path.join(dir, name);
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, source);
  }
  for (const entry of ["packages.cjs", "ticks.cjs"]) {
    const r = spawnSync(process.execPath, [path.join(dir, entry)], { encoding: "utf8" });
    process.stdout.write(r.stdout);
    console.log(`${entry} exit ${r.status}`);
  }
} finally {
  rmSync(dir, { recursive: true, force: true });
}
