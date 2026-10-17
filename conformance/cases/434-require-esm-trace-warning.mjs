// node 22 loads an ES module through require() without a word: no
// ExperimentalWarning is printed or emitted unless --trace-require-module
// asks for one. With `all`, the first require() of an ES module in the
// process emits node's warning, once, however many ES modules follow; with
// `no-node-modules`, a require() made from inside node_modules does not
// count, so the warning names the first one made outside it. The flag needs
// one of those two values (exit 9 otherwise), and execArgv hands it back as
// given.
//
// Only the warning's own lines are compared: the one-time `--trace-warnings`
// hint that follows them names the program (`node` / `oam`), and is case
// 406's to compare.
//
// Regression guard: require() of an ES module threw ERR_REQUIRE_ESM, and the
// flag was refused as unknown.
//
// Each leg runs on the SAME runtime (process.execPath) from a pid-scoped
// fixture directory; the directory is replaced by <D> and separators by /,
// and the (node:PID) prefix is dropped.
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-require-esm-trace-${process.pid}`);
const clean = (s) => String(s).split(dir).join("<D>").split("\\").join("/");
const files = {
  "node_modules/pkg/package.json": '{ "name": "pkg", "main": "index.cjs" }',
  "node_modules/pkg/index.cjs": "module.exports = require('./inner.mjs');",
  "node_modules/pkg/inner.mjs": "export const inner = 1;",
  "a.mjs": "export const a = 1;",
  "b.mjs": "export const b = 2;",
  "main.cjs":
    "let heard = 0;\n" +
    "process.on('warning', (w) => { heard++; });\n" +
    "require('pkg'); require('./a.mjs'); require('./b.mjs');\n" +
    "setTimeout(() => console.log('warning events', heard), 10);",
  "argv.cjs": "console.log(JSON.stringify(process.execArgv));",
};

try {
  for (const [name, source] of Object.entries(files)) {
    const file = path.join(dir, name);
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, source);
  }
  const main = path.join(dir, "main.cjs");
  const leg = (label, args) => {
    const r = spawnSync(process.execPath, args, { encoding: "utf8", cwd: dir });
    console.log(`--- ${label}: exit ${r.status}`);
    process.stdout.write(clean(r.stdout));
    const lines = clean(r.stderr).split(/\r?\n/);
    const at = lines.findIndex((l) => l.includes("ExperimentalWarning"));
    if (at === -1) {
      console.log("no ExperimentalWarning on stderr");
    } else {
      console.log(lines[at].replace(/^\(\w+:\d+\) /, ""));
      console.log(lines[at + 1]);
      const more = lines.slice(at + 2).filter((l) => l.includes("ExperimentalWarning")).length;
      console.log(`further ExperimentalWarning lines: ${more}`);
    }
    return r;
  };
  leg("no flag", [main]);
  leg("--trace-require-module=all", ["--trace-require-module=all", main]);
  leg("--trace-require-module no-node-modules", ["--trace-require-module", "no-node-modules", main]);
  for (const args of [["--trace-require-module"], ["--trace-require-module=yes", "-e", "0"]]) {
    const r = spawnSync(process.execPath, args, { encoding: "utf8" });
    const said = /requires an argument|invalid value/.exec(r.stderr);
    console.log(`--- ${args.join(" ")}: exit ${r.status}, ${said ? said[0] : "no reason given"}`);
  }
  const r = spawnSync(process.execPath, ["--trace-require-module", "all", path.join(dir, "argv.cjs")], {
    encoding: "utf8",
  });
  console.log(`--- execArgv: ${r.stdout.trim()}`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
