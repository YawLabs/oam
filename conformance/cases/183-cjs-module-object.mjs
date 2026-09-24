// A CommonJS `module` carries node's own members: `require`, `path` and
// `paths`. require.main hands the entry's module to every module the program
// loads, so code that reaches for `require.main.require(...)` -- loading
// relative to the application's entry, not the calling file -- or reads
// `require.main.paths` depends on them.
//
// Regression guard: oam's module object held only exports, filename, id and
// loaded. While require.main was undefined, the usual guarded spelling
// (`require.main ? require.main.require(x) : require(x)`) took its fallback;
// once require.main named the entry, the same line threw a TypeError.
//
// Out of scope here: `children` and `parent`, and the main module's id of '.'
// (docs/node-divergences.md). Printed values are booleans, counts and
// module-relative strings, so the compared stdout carries no paths.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// pid-scoped: the runner executes the node and oam legs back to back, and a
// shared fixed path would let one leg's cleanup delete the other's fixtures.
const dir = path.join(os.tmpdir(), `oam-conf-cjs-module-${process.pid}`);
const sub = path.join(dir, "sub");
mkdirSync(sub, { recursive: true });

try {
  // Two files named dep.cjs, one beside the entry and one beside lib, so the
  // answer says which directory a require resolved from.
  writeFileSync(path.join(dir, "dep.cjs"), "module.exports = 'beside the entry';");
  writeFileSync(path.join(sub, "dep.cjs"), "module.exports = 'beside lib';");

  const members =
    "(m, dirname) => ({" +
    " require: typeof m.require," +
    " path: m.path === dirname," +
    " pathsIsArray: Array.isArray(m.paths)," +
    " firstPath: Array.isArray(m.paths) && m.paths[0] === require('path').join(dirname, 'node_modules')," +
    " allNodeModules: Array.isArray(m.paths) && m.paths.every((p) => require('path').basename(p) === 'node_modules')," +
    " pathCount: Array.isArray(m.paths) ? m.paths.length : null })";

  writeFileSync(
    path.join(sub, "lib.cjs"),
    `const members = ${members};` +
      "const load = require.main ? require.main.require.bind(require.main) : require;" +
      "module.exports = {" +
      " own: members(module, __dirname)," +
      " main: require.main ? members(require.main, require('path').dirname(require.main.filename)) : null," +
      " viaMainRequire: load('./dep.cjs')," +
      " viaOwnRequire: require('./dep.cjs')," +
      " moduleRequireIsRequire: module.require('./dep.cjs') === require('./dep.cjs') };",
  );
  writeFileSync(
    path.join(dir, "entry.cjs"),
    "process.stdout.write(JSON.stringify(require('./sub/lib.cjs')) + '\\n');",
  );

  const r = spawnSync(process.execPath, [path.join(dir, "entry.cjs")], { encoding: "utf8" });
  console.log(`cjs entry: exit ${r.status} ${String(r.stdout).trim()}`);
  // The same members on a module a node_modules package loads: its paths
  // skip the node_modules directory it sits in.
  const pkg = path.join(dir, "node_modules", "pkg");
  mkdirSync(pkg, { recursive: true });
  writeFileSync(
    path.join(pkg, "index.js"),
    `module.exports = (${members})(module, __dirname);`,
  );
  writeFileSync(
    path.join(dir, "uses-pkg.cjs"),
    "process.stdout.write(JSON.stringify(require('pkg')) + '\\n');",
  );
  const p = spawnSync(process.execPath, [path.join(dir, "uses-pkg.cjs")], { encoding: "utf8" });
  console.log(`module inside node_modules: exit ${p.status} ${String(p.stdout).trim()}`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
