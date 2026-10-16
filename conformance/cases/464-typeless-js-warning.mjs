// node's MODULE_TYPELESS_PACKAGE_JSON warning: when its syntax test makes a
// typeless .js an ES module and a package.json without "type" scopes the
// file, node warns that it reparsed it -- for the entry, an import and an
// import(); once per package.json, not per file; never for a file under
// node_modules, a file no package.json scopes, or one that stays CommonJS.
// It names the file's URL and the package.json's path, goes through
// process.emitWarning (so a 'warning' listener sees name, code and message,
// and --no-warnings silences the printer but not the listener), and it is no
// deprecation: process.noDeprecation leaves it alone. A "type" node does not
// know is none, and the nearest package.json is the one named.
//
// stdout only is compared, so each run reports what its stderr held: how many
// warnings, and their text with the process id and this run's directory
// taken out. (node's printer adds a trace hint line oam's does not --
// docs/node-divergences.md -- so only the warning's own lines are kept.)
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";

const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-464-")));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(dir, rel)), { recursive: true });
  fs.writeFileSync(path.join(dir, rel), text);
};
const esm = (name) => `export const name = ${JSON.stringify(name)};\n`;
write("pkg/package.json", JSON.stringify({ name: "pkg" }));
write("pkg/a.js", esm("a"));
write("pkg/b.js", esm("b"));
write("pkg/c.js", "module.exports = 'c';\n");
write("pkg/entry.js", "import { name } from './a.js';\nconsole.log('entry', name);\n");
write("pkg/plain-entry.js", "console.log('plain entry', typeof require);\n");
write("pkg/two.mjs", "import './a.js';\nimport './b.js';\nconsole.log('two files, one package');\n");
write("pkg/cjs-only.mjs", "import c from './c.js';\nconsole.log('commonjs', c);\n");
write("other/package.json", JSON.stringify({ name: "other" }));
write("other/d.js", esm("d"));
write("pkg/two-packages.mjs", "import './a.js';\nimport '../other/d.js';\nconsole.log('two packages');\n");
write("pkg/dynamic.mjs", "await import('./a.js');\nawait import('./b.js');\nconsole.log('import() twice');\n");
write(
  "pkg/listener.mjs",
  `const seen = [];
process.on('warning', (w) => seen.push([w.name, w.code, w.message].join(' / ')));
process.on('exit', () => { for (const s of seen) console.log('warning event: ' + s); });
await import('./a.js');
console.log('listener');
`,
);
write("pkg/no-deprecation.mjs", "process.noDeprecation = true;\nawait import('./a.js');\nconsole.log('noDeprecation');\n");
write("pkg/node_modules/dep/package.json", JSON.stringify({ name: "dep", main: "index.js" }));
write("pkg/node_modules/dep/index.js", esm("dep"));
write("pkg/dep.mjs", "import { name } from 'dep';\nconsole.log('node_modules', name);\n");
// No package.json above these (the case's directory has none).
write("loose/e.js", esm("e"));
write("loose/entry.js", "import { name } from './e.js';\nconsole.log('no package.json', name);\n");
write("odd/package.json", JSON.stringify({ type: "cjs" }));
write("odd/entry.js", "export {};\nconsole.log('odd type');\n");
write("parent/package.json", JSON.stringify({ type: "module" }));
write("parent/child/package.json", JSON.stringify({ name: "child" }));
write("parent/child/entry.js", "export {};\nconsole.log('typeless child of a module package');\n");
write("parent/entry.js", "export {};\nconsole.log('module package');\n");

const url = pathToFileURL(dir).href;
const normalize = (text) =>
  text
    .split(url).join("<url>")
    .split(dir).join("<dir>")
    .replace(/\(node:\d+\)/g, "(node:<pid>)")
    .replace(/\\/g, "/");

function run(rel, flags = []) {
  const r = spawnSync(process.execPath, [...flags, rel], { cwd: dir, encoding: "utf8" });
  const lines = r.stderr.split(/\r?\n/);
  const warnings = [];
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].includes("[MODULE_TYPELESS_PACKAGE_JSON]")) warnings.push(lines.slice(i, i + 3).join(" // "));
  }
  console.log(`${[...flags, rel].join(" ")}: ${normalize(r.stdout.trim().split(/\r?\n/).join(" | "))} (status ${r.status})`);
  console.log(`  ${warnings.length} warning(s)`);
  for (const w of warnings) console.log(`  ${normalize(w)}`);
}
try {
  run("pkg/entry.js");
  run("pkg/plain-entry.js");
  run("pkg/two.mjs");
  run("pkg/cjs-only.mjs");
  run("pkg/two-packages.mjs");
  run("pkg/dynamic.mjs");
  run("pkg/listener.mjs");
  run("pkg/listener.mjs", ["--no-warnings"]);
  run("pkg/two.mjs", ["--no-warnings"]);
  run("pkg/no-deprecation.mjs");
  run("pkg/two.mjs", ["--no-deprecation"]);
  run("pkg/dep.mjs");
  run("loose/entry.js");
  run("odd/entry.js");
  run("parent/child/entry.js");
  run("parent/entry.js");
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}
