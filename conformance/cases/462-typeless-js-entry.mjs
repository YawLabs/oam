// A typeless .js program entry: a .js file whose nearest package.json has no
// "type" -- or that has no package.json above it at all -- is CommonJS unless
// it has module syntax (node's containsModuleSyntax): an import or export
// statement, import.meta, a top-level await, or a top-level declaration of a
// name the CommonJS wrapper owns. With module syntax it is an ES module, so a
// file that mixes import with require() fails on require. A "type" other than
// "module" or "commonjs" is none to node, and the nearest package.json decides,
// whatever a parent's says.
//
// Regression guard: oam ran every typeless project .js as an ES module
// (ESM-first), so a plain script lost require, module and its sloppy-mode
// top level, and `this` was undefined.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-462-"));
const shapes = {
  "plain.js": "console.log(typeof require, typeof module, typeof exports, typeof __filename);\n",
  "this.js": "console.log(this === module.exports, typeof this);\n",
  "require.js": "const p = require('node:path');\nconsole.log(typeof p.join, require.main === module);\n",
  "sloppy.js": "x = 5;\nconsole.log(x, (function () { return this === undefined; })());\n",
  "with.js": "with ({ a: 1 }) { console.log('with', a); }\n",
  "return.js": "console.log('before');\nif (true) return;\nconsole.log('after');\n",
  "import.js": "import p from 'node:path';\nconsole.log(typeof p.join, typeof require);\n",
  "export.js": "export const v = 1;\nconsole.log('export', typeof module);\n",
  "export-empty.js": "export {};\nconsole.log('export {}', this === undefined);\n",
  "meta.js": "console.log(typeof import.meta.url, typeof require);\n",
  "meta-in-string.js": "console.log('import.meta', typeof require);\n",
  "tla.js": "await Promise.resolve();\nconsole.log('top-level await', typeof require);\n",
  "await-in-call.js": "console.log(await Promise.resolve('awaited'), typeof module);\n",
  "await-identifier.js": "var await = 3;\nconsole.log('await is', await);\n",
  "redeclare-require.js": "const require = 1;\nconsole.log('const require', require, typeof module);\n",
  // `global` is a runtime global, not a CommonJS wrapper parameter (node's
  // wrapper is the five names), so a top-level binding of it must run. `let`
  // and `class` share the root cause with `const` (a redeclaration of the
  // wrapper's name) and node accepts all three.
  "global-binding.js": "const global = 1;\nconsole.log('const global', global, typeof require);\n",
  "global-let.js": "let global = 2;\nconsole.log('let global', global, typeof module);\n",
  "global-class.js": "class global {}\nconsole.log('class global', typeof global, typeof require);\n",
  "mixed.js": "import p from 'node:path';\nconst fs = require('node:fs');\nconsole.log('mixed', typeof fs);\n",
  "shebang.js": "#!/usr/bin/env node\nconsole.log('shebang', typeof require);\n",
  "dynamic-import.js": "import('node:path').then((p) => console.log('import()', typeof p.join, typeof require));\n",
};
// Where the files sit: the package.json above each, if any.
const scopes = {
  "no-package-json": null,
  "typeless-package": { name: "t" },
  "odd-type": { type: "cjs" },
};
for (const [scope, manifest] of Object.entries(scopes)) {
  fs.mkdirSync(path.join(dir, scope));
  if (manifest) fs.writeFileSync(path.join(dir, scope, "package.json"), JSON.stringify(manifest));
  for (const [name, text] of Object.entries(shapes)) fs.writeFileSync(path.join(dir, scope, name), text);
}
// A typeless package inside a "type": "module" one: the nearest decides.
fs.mkdirSync(path.join(dir, "module-parent", "child"), { recursive: true });
fs.writeFileSync(path.join(dir, "module-parent", "package.json"), JSON.stringify({ type: "module" }));
fs.writeFileSync(path.join(dir, "module-parent", "child", "package.json"), JSON.stringify({ name: "c" }));
fs.writeFileSync(path.join(dir, "module-parent", "child", "plain.js"), shapes["plain.js"]);
fs.writeFileSync(path.join(dir, "module-parent", "child", "import.js"), shapes["import.js"]);
fs.writeFileSync(path.join(dir, "module-parent", "plain.js"), "console.log(typeof require, this === undefined);\n");
// And a "type": "commonjs" one around a typeless one.
fs.mkdirSync(path.join(dir, "commonjs-parent", "child"), { recursive: true });
fs.writeFileSync(path.join(dir, "commonjs-parent", "package.json"), JSON.stringify({ type: "commonjs" }));
fs.writeFileSync(path.join(dir, "commonjs-parent", "child", "package.json"), "{}");
fs.writeFileSync(path.join(dir, "commonjs-parent", "child", "import.js"), shapes["import.js"]);
fs.writeFileSync(path.join(dir, "commonjs-parent", "import.js"), shapes["import.js"]);
// A package.json sitting directly in a node_modules directory is never read
// (node's TraverseParent stops at the boundary; GetPackageScopeConfig refuses a
// `/node_modules/package.json`), so the file is decided by its own syntax.
fs.mkdirSync(path.join(dir, "nm-commonjs", "node_modules"), { recursive: true });
fs.writeFileSync(path.join(dir, "nm-commonjs", "node_modules", "package.json"), JSON.stringify({ type: "commonjs" }));
fs.writeFileSync(path.join(dir, "nm-commonjs", "node_modules", "dep.js"), "export const v = 1;\nconsole.log('nm esm', typeof require);\n");
fs.mkdirSync(path.join(dir, "nm-module", "node_modules"), { recursive: true });
fs.writeFileSync(path.join(dir, "nm-module", "node_modules", "package.json"), JSON.stringify({ type: "module" }));
fs.writeFileSync(path.join(dir, "nm-module", "node_modules", "dep.js"), "console.log('nm cjs', typeof require);\n");
// A real package under node_modules still reads its own package.json.
fs.mkdirSync(path.join(dir, "nm-module", "node_modules", "pkg"), { recursive: true });
fs.writeFileSync(path.join(dir, "nm-module", "node_modules", "pkg", "package.json"), JSON.stringify({ type: "module" }));
fs.writeFileSync(path.join(dir, "nm-module", "node_modules", "pkg", "index.js"), "console.log('pkg esm', typeof require);\n");
// import.meta.url's percent-encoding matches node's ada path encoder: a
// `[id].js`-style name encodes `[`/`]`, and `~`/space too. Prints only the
// basename, so no absolute path reaches the output.
fs.mkdirSync(path.join(dir, "url-names"));
fs.writeFileSync(
  path.join(dir, "url-names", "a[b~ c].js"),
  "console.log(import.meta.url.slice(import.meta.url.lastIndexOf('/') + 1));\n",
);

function run(rel) {
  const r = spawnSync(process.execPath, [rel], { cwd: dir, encoding: "utf8" });
  const out = r.stdout.trim().split(/\r?\n/).filter(Boolean).join(" | ");
  // What failed, by kind: the stderr wording is each runtime's own.
  const failure = r.status === 0 ? "" : ` (status ${r.status}${/require is not defined/.test(r.stderr) ? ": require is not defined" : ""})`;
  return `${out}${failure}`;
}
try {
  for (const scope of Object.keys(scopes)) {
    for (const name of Object.keys(shapes)) {
      console.log(`${scope}/${name}: ${run(path.join(scope, name))}`);
    }
  }
  for (const rel of [
    "module-parent/plain.js",
    "module-parent/child/plain.js",
    "module-parent/child/import.js",
    "commonjs-parent/import.js",
    "commonjs-parent/child/import.js",
    "nm-commonjs/node_modules/dep.js",
    "nm-module/node_modules/dep.js",
    "nm-module/node_modules/pkg/index.js",
    "url-names/a[b~ c].js",
  ]) {
    console.log(`${rel}: ${run(rel)}`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
