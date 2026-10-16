// How node 22's require() of an ES module fails when the graph cannot load,
// every time it is asked:
//
// - an import that does not resolve is ERR_MODULE_NOT_FOUND, `Cannot find
//   module '<file>' imported from <importer>` with the file's URL as `url`
//   for a relative specifier, `Cannot find package '<name>' imported from
//   <importer>` for a bare one -- and a second require() fails the same
//   way. (oam threw its own OAM-MOD0001/OAM-MOD0002 diagnostics, and the
//   second require() an internal "module edge not preloaded" error.)
// - a CommonJS module the graph imports that throws runs ONCE: the graph
//   stays errored, and every later require() or import() of it throws that
//   same error object without running the module again. (oam ran the
//   CommonJS body again on each attempt and threw a new error each time.)
//
// The entry runs on the SAME runtime (process.execPath) from a pid-scoped
// fixture directory; the directory is replaced by <D> and separators by /.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-load-failures-${process.pid}`);
const files = {
  "missing.mjs": "import './nope.mjs';\n",
  "missingpkg.mjs": "import 'no-such-package-here';\n",
  "t.cjs": "console.log('t.cjs body ran');\nthrow new TypeError('t.cjs threw');\n",
  "cjsthrow.mjs": "import './t.cjs';\nexport const unreachable = 1;\n",
  // A TYPELESS .js with module syntax: require() decides it by its syntax
  // (the Detect path), and its graph cannot load. Its own failing import is
  // what node reports -- not a silent undefined from a dropped exception.
  "broken.js": "import './nope.mjs';\nexport const v = 1;\n",
  "main.cjs": `
const clean = (s) => String(s).split(__dirname).join('<D>').split('\\\\').join('/')
  .split(require('url').pathToFileURL(__dirname).href).join('<URL>');
const attempt = (label, specifier) => {
  try {
    require(specifier);
    console.log(label, 'loaded');
  } catch (e) {
    console.log(label, e.code, e.name, JSON.stringify(Object.keys(e)));
    console.log(' ', JSON.stringify(clean(e.message)), e.url === undefined ? '' : clean(e.url));
    return e;
  }
};
attempt('missing module', './missing.mjs');
attempt('missing module again', './missing.mjs');
attempt('missing package', './missingpkg.mjs');
attempt('missing package again', './missingpkg.mjs');
attempt('typeless js with module syntax', './broken.js');
attempt('typeless js again', './broken.js');
const first = attempt('commonjs dependency threw', './cjsthrow.mjs');
const second = attempt('again', './cjsthrow.mjs');
console.log('same error object', first === second, first instanceof TypeError);
import('./cjsthrow.mjs').catch((e) => console.log('import() too: same error object', e === first));
`,
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
