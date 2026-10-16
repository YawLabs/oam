// A typeless .js file reached from another module is decided as node decides
// an entry: CommonJS unless it has module syntax. From an ES module, a static
// import and import() of a CommonJS one get its module.exports as the
// default; of one with an export, its named exports. From CommonJS,
// require() and a createRequire() require of a CommonJS one get its
// module.exports, and import() of one with an export gets the module. A tool
// in node_modules that require()s a project's typeless config file
// (`module.exports = {...}`) gets the object -- the shape that failed. Inside
// node_modules node applies the same test: a typeless package whose entry has
// an export is an ES module to import.
//
// require() of a typeless file with module syntax is left out: node 22 loads
// it (require(esm)), oam throws ERR_REQUIRE_ESM (docs/node-divergences.md).
//
// Regression guard: oam ran a typeless project .js as an ES module, so
// require() of `module.exports = {...}` threw ERR_REQUIRE_ESM and an import of
// it saw no module.exports; a typeless node_modules entry with an export was
// run as CommonJS and threw a SyntaxError.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-463-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
  fs.writeFileSync(path.join(dir, rel), text);
};
write("app/package.json", JSON.stringify({ name: "app" }));
write("app/cjs.js", "module.exports = { kind: 'cjs', require: typeof require };\nconsole.log('cjs.js ran');\n");
write("app/esm.js", "export const kind = 'esm';\nexport const req = typeof require;\nconsole.log('esm.js ran');\n");
write("app/plain.js", "console.log('plain.js ran', typeof module);\n");
write("app/my.config.js", "module.exports = { answer: 42 };\n");
write("app/node_modules/tool/package.json", JSON.stringify({ name: "tool", main: "index.js" }));
write("app/node_modules/tool/index.js", "module.exports = (p) => require(p);\n");
write("app/node_modules/esm-dep/package.json", JSON.stringify({ name: "esm-dep", main: "index.js" }));
write("app/node_modules/esm-dep/index.js", "export const v = 'esm-dep';\nexport default typeof require;\n");
write("app/node_modules/cjs-dep/package.json", JSON.stringify({ name: "cjs-dep" }));
write("app/node_modules/cjs-dep/index.js", "module.exports = { v: 'cjs-dep', module: typeof module };\n");
write(
  "app/from-esm.mjs",
  `import cjs from './cjs.js';
import { kind, req } from './esm.js';
console.log('import cjs', JSON.stringify(cjs));
console.log('import esm', kind, req);
const plain = await import('./plain.js');
console.log('import() plain', Object.keys(plain).join(','));
const esm = await import('./esm.js');
console.log('import() esm', Object.keys(esm).sort().join(','));
const dep = await import('esm-dep');
console.log('import esm-dep', dep.v, dep.default);
const cdep = await import('cjs-dep');
console.log('import cjs-dep', JSON.stringify(cdep.default));
`,
);
write(
  "app/from-cjs.cjs",
  `console.log('require cjs', JSON.stringify(require('./cjs.js')));
console.log('require again is cached', require('./cjs.js') === require('./cjs.js'));
const { createRequire } = require('node:module');
const r = createRequire(__filename);
console.log('createRequire cjs', r('./cjs.js').kind);
console.log('require cjs-dep', JSON.stringify(require('cjs-dep')));
import('./esm.js').then((m) => console.log('import() esm from cjs', m.kind, m.req));
`,
);
write(
  "app/run-tool.cjs",
  "const load = require('tool');\nconsole.log(load(require('node:path').resolve(__dirname, 'my.config.js')));\n",
);
write("app/run-tool.mjs", "import load from 'tool';\nimport path from 'node:path';\nconsole.log(load(path.resolve(import.meta.dirname, 'my.config.js')));\n");
try {
  for (const file of ["from-esm.mjs", "from-cjs.cjs", "run-tool.cjs", "run-tool.mjs"]) {
    const r = spawnSync(process.execPath, [file], { cwd: path.join(dir, "app"), encoding: "utf8" });
    console.log(`${file}: ${r.stdout.trim().split(/\r?\n/).join(" | ")} (status ${r.status})`);
  }
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
