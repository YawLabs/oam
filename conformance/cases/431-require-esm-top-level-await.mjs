// require() of an ES module graph with a top-level await anywhere in it --
// the required module itself or a module it imports, however deep -- is
// node 22's ERR_REQUIRE_ASYNC_MODULE: the graph is instantiated, found
// async, and refused before ANY of it runs (not even the synchronous
// modules ahead of the await), with node's message naming the requiring and
// the required file. It is refused the same way on every require(), and an
// import() of the same module afterwards still evaluates it -- after which
// a require() is STILL refused: node tests the graph for a top-level await
// whatever state import() left it in, so an evaluated async graph is no
// more requirable than one that never ran.
//
// Regression guard: oam threw ERR_REQUIRE_ESM for every require() of an ES
// module, async or not.
//
// The entry runs on the SAME runtime (process.execPath) from a pid-scoped
// fixture directory; the directory is replaced by <D> and separators by /,
// so stdout carries no paths.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-tla-${process.pid}`);
const files = {
  "tla.mjs": "console.log('tla body ran'); await 0; export const x = 1;",
  "sync-first.mjs": "console.log('sync-first ran'); export const s = 1;",
  "deep.mjs":
    "import './sync-first.mjs'; import './middle.mjs';\n" +
    "console.log('deep ran'); export const y = 2;",
  "middle.mjs": "import { x } from './tla.mjs'; export const m = x;",
  "main.cjs": `
const clean = (s) => String(s).split(__dirname).join('<D>').split('\\\\').join('/');
const attempt = (label, specifier) => {
  try {
    require(specifier);
    console.log(label, 'loaded');
  } catch (e) {
    console.log(label, e.code, e.name, e instanceof Error);
    console.log(JSON.stringify(clean(e.message)));
    return e;
  }
};
attempt('direct', './tla.mjs');
attempt('direct again', './tla.mjs');
attempt('deep', './deep.mjs');
import('./deep.mjs').then((ns) => {
  console.log('import() afterwards', ns.y);
  attempt('after import() finished', './deep.mjs');
  attempt('after import() finished, direct', './tla.mjs');
});
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
