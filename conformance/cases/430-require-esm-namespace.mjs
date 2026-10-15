// require() of an ES module, as node 22 does it by default: the graph loads
// and evaluates synchronously inside the require() and the call returns the
// module namespace. What a CommonJS caller sees is node's loadESMFromCJS:
// - the namespace itself (toStringTag Module, not extensible, the same
//   object on every require and from import());
// - with a default export and no own __esModule, a facade namespace that
//   adds an enumerable `__esModule: true`, so transpiled interop
//   (`m.__esModule ? m.default : m`) finds the default; without a default
//   export, no __esModule at all; an own `__esModule` export is kept;
// - an export named 'module.exports' is returned in place of the namespace;
// and process.features.require_module says so.
//
// Which files are ES modules is node's rule: .mjs, a .js under
// "type": "module" (in node_modules too), and a .js with no declared type
// whose syntax only an ES module has.
//
// Regression guard: every require() of an ES module threw ERR_REQUIRE_ESM
// while process.features.require_module claimed true.
//
// Each leg runs a CommonJS entry on the SAME runtime (process.execPath); the
// fixture directory is pid-scoped (the runner executes the node and oam legs
// back to back) and stdout carries no paths.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-ns-${process.pid}`);
const files = {
  "dep.mjs": "export const dep = 'D';",
  "c.cjs": "module.exports = 'C';",
  "esm.mjs":
    "import { dep } from './dep.mjs'; import c from './c.cjs';\n" +
    "export const named = 1; export { dep }; export const cjsdep = c;\n" +
    "export default 'def';",
  "nodefault.mjs": "export const a = 1;",
  "own-esmodule.mjs": "export const __esModule = false; export default 1;",
  "fn.mjs":
    "function f() { return 'fn called'; }\n" +
    "export { f as 'module.exports' }; export const other = 1;",
  "typed/package.json": '{ "type": "module" }',
  "typed/x.js": "export const v = 'typed';",
  "detected.js": "import { dep } from './dep.mjs'; export const v = 'detected ' + dep;",
  "plain.js": "module.exports = { plain: true };",
  "node_modules/esm-pkg/package.json":
    '{ "name": "esm-pkg", "type": "module", "exports": "./index.js" }',
  "node_modules/esm-pkg/index.js": "export const pkg = 'from node_modules';",
  "counter.mjs": "globalThis.__evaluations = (globalThis.__evaluations || 0) + 1;\nexport const n = 1;",
  "main.cjs": `
const show = (label, v) => console.log(label, JSON.stringify(v));
const ns = require('./esm.mjs');
show('keys', Object.keys(ns));
show('values', [ns.default, ns.named, ns.dep, ns.cjsdep, ns.__esModule]);
show('tag', Object.prototype.toString.call(ns));
show('extensible', Object.isExtensible(ns));
show('same on a second require', require('./esm.mjs') === ns);
const desc = Object.getOwnPropertyDescriptor(ns, '__esModule');
show('__esModule descriptor', [desc.value, desc.enumerable, desc.writable]);
const nd = require('./nodefault.mjs');
show('no default: keys', Object.keys(nd));
show('no default: __esModule in', '__esModule' in nd);
const own = require('./own-esmodule.mjs');
show('own __esModule', [own.__esModule, Object.keys(own)]);
const fn = require('./fn.mjs');
show('module.exports export', [typeof fn, fn()]);
show('type module .js', require('./typed/x.js').v);
show('detected .js', require('./detected.js').v);
show('plain commonjs .js', require('./plain.js'));
show('node_modules', require('esm-pkg').pkg);
show('features.require_module', process.features.require_module);
const counter = require('./counter.mjs');
import('./counter.mjs').then((viaImport) => {
  show('import() gives the same namespace', viaImport === counter);
  show('evaluated once', globalThis.__evaluations);
});
`,
};

try {
  for (const [name, source] of Object.entries(files)) {
    const file = path.join(dir, name);
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, source);
  }
  const r = spawnSync(process.execPath, [path.join(dir, "main.cjs")], { encoding: "utf8" });
  process.stdout.write(r.stdout);
  console.log(`exit ${r.status}`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
