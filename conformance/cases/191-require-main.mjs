// require.main names the program's entry module, the way node's CommonJS
// loader does. `if (require.main === module) main()` is how a CommonJS script
// tells "I was run" from "I was required", so it is the entry point of most
// CLI tools and hook scripts written for node.
//
// Regression guard: oam never set require.main, so the guard was always false
// and such a script loaded, ran nothing, and exited 0. A PreToolUse hook that
// exists to DENY a command answered with silence, which a harness reads as
// "allow" -- the script failed open, and nothing on the command line said so.
//
// Each leg spawns an entry on the SAME runtime (process.execPath) and prints
// only booleans, types and basenames, so the compared stdout carries no paths.
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// pid-scoped: the runner executes the node and oam legs back to back, and a
// shared fixed path would let one leg's cleanup delete the other's fixtures.
const dir = path.join(os.tmpdir(), `oam-conf-require-main-${process.pid}`);
const typed = path.join(dir, "typed-commonjs");
const sub = path.join(typed, "sub");
mkdirSync(sub, { recursive: true });

try {
  // Reached from the entry through lib, so require.main has to hold two levels
  // down, not only in the entry's own require.
  const deep =
    "exports.report = () => ({" +
    " deepSeesEntry: require.main === globalThis.__entry," +
    " deepIsMain: require.main === module });";

  // Carries the guard too: required, not run, so its guard must stay false.
  const lib =
    "let libGuardRan = false;" +
    "if (require.main === module) libGuardRan = true;" +
    "exports.report = () => ({" +
    " libGuardRan," +
    " libIsMain: require.main === module," +
    " libSeesEntry: require.main === globalThis.__entry," +
    " libSeesEntryFile: require.main === undefined ? null : require('path').basename(require.main.filename)," +
    " ...require('./deep.cjs').report() });";

  const entry =
    "globalThis.__entry = module;" +
    "const r = { guardRan: false };" +
    "if (require.main === module) r.guardRan = true;" +
    "r.entryIsMain = require.main === module;" +
    "r.mainFilenameIsOwn = require.main !== undefined && require.main.filename === __filename;" +
    "r.processMainModuleIsEntry = process.mainModule === module;" +
    "Object.assign(r, require('./lib.cjs').report());" +
    "r.sameMainOnSecondRequire = require('./lib.cjs').report().libSeesEntry;" +
    "process.stdout.write(JSON.stringify(r) + '\\n');";

  // The smallest report, for legs where one entry is the whole question.
  const report =
    "JSON.stringify({ isMain: require.main === module," +
    " processMainModuleIsEntry: process.mainModule === module })";

  for (const d of [dir, typed]) {
    writeFileSync(path.join(d, "deep.cjs"), deep);
    writeFileSync(path.join(d, "lib.cjs"), lib);
  }
  writeFileSync(path.join(dir, "entry.cjs"), entry);
  // A typeless `.js` entry under a package.json that declares "commonjs". In
  // node a typeless .js is CommonJS anyway; oam's typeless default is ESM
  // (docs/node-divergences.md, entry 7), so a directory of plain `.js`
  // CommonJS scripts -- hooks run as `<runtime> <script>` -- has to declare it.
  writeFileSync(path.join(typed, "package.json"), '{ "type": "commonjs" }\n');
  writeFileSync(path.join(typed, "entry.js"), entry);

  const leg = (label, args, cwd) => {
    const r = spawnSync(process.execPath, args, { encoding: "utf8", cwd });
    console.log(`${label}: exit ${r.status} ${String(r.stdout).trim()}`);
  };
  leg("cjs entry", [path.join(dir, "entry.cjs")]);
  leg("typeless .js entry under type commonjs", [path.join(typed, "entry.js")]);

  // The same typeless .js named RELATIVE to a working directory below the
  // package.json that makes it CommonJS: the type lookup has to walk up from
  // the file's real location, not stop at the working directory.
  writeFileSync(path.join(sub, "rel.js"), `process.stdout.write(${report} + '\\n');`);
  leg("relative typeless .js entry from a subdirectory", ["rel.js"], sub);

  // A worker's CommonJS entry and a forked script are each the main module of
  // the thread or process they start, so the same guard has to run there. Each
  // reports back over its channel and the launcher prints, so the compared
  // stdout never interleaves two writers.
  writeFileSync(
    path.join(dir, "worker-entry.cjs"),
    `require('worker_threads').parentPort.postMessage(${report});`,
  );
  writeFileSync(
    path.join(dir, "worker-launcher.cjs"),
    "const { Worker } = require('worker_threads');" +
      "new Worker(require('path').join(__dirname, 'worker-entry.cjs'))" +
      ".on('message', (m) => process.stdout.write(m + '\\n'));",
  );
  leg("worker cjs entry", [path.join(dir, "worker-launcher.cjs")]);
  writeFileSync(path.join(dir, "fork-entry.cjs"), `process.send(${report});`);
  const forkLauncher = (target) =>
    `const c = require('child_process').fork(${target});` +
    "c.on('message', (m) => { process.stdout.write(m + '\\n'); c.disconnect(); });";
  writeFileSync(
    path.join(dir, "fork-launcher.cjs"),
    forkLauncher("require('path').join(__dirname, 'fork-entry.cjs')"),
  );
  leg("forked cjs script", [path.join(dir, "fork-launcher.cjs")]);
  // fork() of a relative typeless .js, from the subdirectory again.
  writeFileSync(path.join(sub, "rel-fork-entry.js"), `process.send(${report});`);
  writeFileSync(path.join(sub, "rel-fork-launcher.cjs"), forkLauncher("'rel-fork-entry.js'"));
  leg("forked relative typeless .js from a subdirectory", ["rel-fork-launcher.cjs"], sub);

  // The main module outlives a throw from the entry's body: an
  // 'uncaughtException' listener still sees it, and handling it exits 0.
  writeFileSync(
    path.join(dir, "throws.cjs"),
    "process.on('uncaughtException', () => process.stdout.write(JSON.stringify({" +
      " mainKeptAfterThrow: require.main === module && process.mainModule === module }) + '\\n'));" +
      "throw new Error('entry body threw');",
  );
  leg("entry that throws, with an uncaughtException listener", [path.join(dir, "throws.cjs")]);

  // node builds every require with `require.main = process.mainModule`, so a
  // module required after the program reassigns process.mainModule sees the
  // new value.
  writeFileSync(
    path.join(dir, "late.cjs"),
    "module.exports = require.main ? require('path').basename(String(require.main.filename)) : null;",
  );
  writeFileSync(
    path.join(dir, "reassign.cjs"),
    "process.mainModule = { filename: 'reassigned' };" +
      "process.stdout.write(JSON.stringify({ laterRequireSees: require('./late.cjs') }) + '\\n');",
  );
  leg("process.mainModule reassigned before a require", [path.join(dir, "reassign.cjs")]);

  // -e and -p source is not a module file: node has no main module there.
  leg("eval", ["-e", "process.stdout.write(typeof require.main + ' ' + typeof process.mainModule)"]);
  leg("print", ["-p", "typeof require.main + ' ' + typeof process.mainModule"]);

  // This file is an ES module entry: node has no main CommonJS module then, so
  // require.main is undefined on a createRequire require and on every CommonJS
  // module that require loads.
  const req = createRequire(import.meta.url);
  const viaEsm = req(path.join(dir, "lib.cjs")).report();
  console.log(
    `esm entry: createRequire main ${typeof req.main}, ` +
      `required module sees main ${viaEsm.libSeesEntryFile === null ? "undefined" : viaEsm.libSeesEntryFile}, ` +
      `its guard ran ${viaEsm.libGuardRan}, process.mainModule ${typeof process.mainModule}`,
  );
} finally {
  rmSync(dir, { recursive: true, force: true });
}
