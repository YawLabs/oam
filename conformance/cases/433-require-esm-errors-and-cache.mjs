// What node 22's require() of an ES module leaves behind:
// - an error thrown while the graph evaluates is rethrown, and every later
//   require() of the module rethrows that SAME error object (the module is
//   errored for good, as for import());
// - a module that loaded is in require.cache under its filename, with
//   `exports` the value require() returned and `loaded` true, next to the
//   CommonJS and JSON modules; require.cache is node:module's _cache and has
//   no prototype;
// - deleting a CommonJS entry reloads the file on the next require();
//   deleting an ES module's entry does not re-evaluate the module, but a
//   default-export module gets a new facade (not ===), while one without a
//   default export hands back the very same namespace;
// - an entry the program plants is what require() returns.
//
// Regression guard: require() of an ES module threw ERR_REQUIRE_ESM, and
// require.cache was a fresh empty object on every require -- it listed
// nothing, and deleting or planting an entry did nothing.
//
// The entry runs on the SAME runtime (process.execPath) from a pid-scoped
// fixture directory; the directory is replaced by <D> and separators by /.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-cache-${process.pid}`);
const files = {
  "boom.mjs": "console.log('boom body ran');\nthrow new Error('boom');",
  "c.cjs": "console.log('c body ran');\nmodule.exports = { c: 1 };",
  "e.mjs": "console.log('e body ran');\nexport default 1;",
  "w.mjs": "export const w = 1;",
  "j.json": '{ "json": true }',
  // On disk, because require() resolves the file before it asks the cache.
  "planted.cjs": "module.exports = 'from disk';",
  "main.cjs": `
const clean = (s) => String(s).split(__dirname).join('<D>').split('\\\\').join('/');
const show = (label, v) => console.log(label, JSON.stringify(v));
let first;
try { require('./boom.mjs'); } catch (e) { first = e; show('evaluation error', e.message); }
try { require('./boom.mjs'); } catch (e) { show('rethrown, same object', e === first); }
show('errored module in require.cache', require.resolve('./boom.mjs') in require.cache);

show('no prototype', Object.getPrototypeOf(require.cache) === null);
show('is Module._cache', require.cache === require('module')._cache);

const c1 = require('./c.cjs');
delete require.cache[require.resolve('./c.cjs')];
show('commonjs reloaded after delete', require('./c.cjs') !== c1);

const e1 = require('./e.mjs');
const entry = require.cache[require.resolve('./e.mjs')];
show('esm entry', [typeof entry, entry.loaded, entry.exports === e1, entry.filename === entry.id]);
delete require.cache[require.resolve('./e.mjs')];
const e2 = require('./e.mjs');
show('default facade after delete: new, same values', [e1 === e2, e1.default, e2.default]);
const w1 = require('./w.mjs');
delete require.cache[require.resolve('./w.mjs')];
show('no default after delete: same namespace', require('./w.mjs') === w1);

require.cache[require('path').join(__dirname, 'planted.cjs')] = { exports: 'PLANTED', loaded: true };
show('planted entry', require('./planted.cjs'));
require('./j.json');
show('listed', Object.keys(require.cache).map(clean));
// The same planted entry, seen through the ESM side: an import() of a .cjs
// goes through the generated facade, whose __oam.requireCjs reads the shared
// require.cache first, so the import gets the planted exports too (node).
import('./uses-planted.mjs').catch((e) => show('import error', e.message));
`,
  "uses-planted.mjs": "import x from './planted.cjs';\nconsole.log('import of planted dep:', JSON.stringify(x));\n",
};

try {
  mkdirSync(dir, { recursive: true });
  for (const [name, source] of Object.entries(files)) {
    writeFileSync(path.join(dir, name), source);
  }
  const r = spawnSync(process.execPath, [path.join(dir, "main.cjs")], { encoding: "utf8" });
  process.stdout.write(r.stdout);
  console.log(`exit ${r.status}`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
