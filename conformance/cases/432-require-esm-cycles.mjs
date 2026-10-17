// require() of an ES module in a cycle, as node 22 refuses it: when the
// module, or a module its graph imports, is still being loaded or evaluated,
// the require() throws ERR_REQUIRE_CYCLE_MODULE instead of handing back a
// half-evaluated namespace (which would break the ECMAScript invariants
// node keeps). The thrower can catch it and the outer load goes on. Each
// way in has its own wording in node, and each is pinned:
// - an ES entry imports a CommonJS module that requires the entry back (the
//   long form, with node's advice);
// - a CommonJS main requires an ES module whose CommonJS dependency
//   requires it back (the short form, naming the module that started the
//   load);
// - a CommonJS dependency requires a FRESH ES module whose graph imports the
//   one in evaluation ("Cannot import Module");
// - a CommonJS main whose own ES dependency imports the main back while the
//   main's body is running ("Cannot import CommonJS Module").
// A cycle among ES modules only, entered through require(), is not one of
// these: the graph loads and evaluates as import() would.
//
// Regression guard: oam threw ERR_REQUIRE_ESM for every require() of an ES
// module, so none of these got as far as the cycle question.
//
// Each leg runs on the SAME runtime (process.execPath) from a pid-scoped
// fixture directory; the directory is replaced by <D> and separators by /.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-cycle-${process.pid}`);
const clean = (s) => String(s).split(dir).join("<D>").split("\\").join("/");
// The CommonJS side of each cycle reports what it caught and carries on.
const report =
  "const clean = (s) => String(s).split(__dirname).join('<D>').split('\\\\').join('/');\n" +
  "const tryRequire = (label, specifier) => {\n" +
  "  try { const r = require(specifier); console.log(label, 'got', JSON.stringify(Object.keys(r))); }\n" +
  "  catch (e) { console.log(label, e.code, JSON.stringify(clean(e.message))); }\n" +
  "};\n";
const legs = {
  "esm-entry": {
    "a.mjs":
      "import b from './b.cjs';\nexport const A = 'A';\nconsole.log('a ran, b =', JSON.stringify(b));",
    "b.cjs": report + "tryRequire('b', './a.mjs');\nmodule.exports = { b: 1 };",
    entry: "a.mjs",
  },
  "cjs-main": {
    "main.cjs": report + "tryRequire('main', './a.mjs');",
    "a.mjs":
      "import b from './b.cjs';\nexport const A = 'A';\nconsole.log('a ran, b =', JSON.stringify(b));",
    "b.cjs": report + "tryRequire('b', './a.mjs');\nmodule.exports = { b: 1 };",
    entry: "main.cjs",
  },
  "fresh-module-reaching-back": {
    "main.cjs": report + "tryRequire('main', './x.mjs');",
    "x.mjs":
      "import c from './c.cjs';\nexport const X = 'X';\nconsole.log('x ran, c =', JSON.stringify(c));",
    "c.cjs": report + "tryRequire('c', './y.mjs');\nmodule.exports = { c: 1 };",
    "y.mjs": "import { X } from './x.mjs';\nexport const Y = 'Y';",
    entry: "main.cjs",
  },
  "esm-importing-running-main": {
    "main.cjs": report + "exports.early = 1;\ntryRequire('main', './e.mjs');",
    "e.mjs": "import m from './main.cjs';\nconsole.log('e ran', JSON.stringify(m));",
    entry: "main.cjs",
  },
  "esm-only-cycle": {
    "main.cjs":
      report + "tryRequire('main', './x.mjs');\nconsole.log('values', require('./x.mjs').X, require('./x.mjs').fromY);",
    "x.mjs": "import { Y } from './y.mjs';\nexport const X = 'X';\nexport const fromY = Y;",
    "y.mjs": "import { X } from './x.mjs';\nexport const Y = 'Y';\nexport const getX = () => X;",
    entry: "main.cjs",
  },
};

try {
  for (const [name, leg] of Object.entries(legs)) {
    const legDir = path.join(dir, name);
    mkdirSync(legDir, { recursive: true });
    for (const [file, source] of Object.entries(leg)) {
      if (file !== "entry") writeFileSync(path.join(legDir, file), source);
    }
    const r = spawnSync(process.execPath, [path.join(legDir, leg.entry)], { encoding: "utf8" });
    console.log(`--- ${name}: exit ${r.status}`);
    process.stdout.write(clean(r.stdout));
  }
} finally {
  rmSync(dir, { recursive: true, force: true });
}
