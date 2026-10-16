// A `*` in an fs grant entry ends it: what comes before is a prefix of the
// paths it admits (#376). `--allow-fs-read=<dir>/*` (and `<dir>\*` on
// Windows) grants the directory and everything under it; `<dir>*` grants
// every path that starts with `<dir>`, `<dir>x` and `<dir>.txt` too; anything
// after the first `*` is ignored (`<dir>/*/f.txt` is `<dir>/*`); a root
// followed by `*` (`/*`, `\*`, `C:\*`) grants the whole tree of that root.
// The same holds for `--allow-fs-write`. On POSIX `\` is a filename
// character, not a separator, so `<dir>\*` grants nothing under `<dir>`.
//
// Regression guard: oam matched the entry literally, so `<dir>\*` and
// `<dir>/*` granted only a file named `*`, and a run that gave them was
// refused every read and write under `<dir>`.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";

const D = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-511-")));
const C = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-511-child-")));
process.on("exit", () => {
  fs.rmSync(D, { recursive: true, force: true });
  fs.rmSync(C, { recursive: true, force: true });
});
for (const d of ["T", "T/sub", "Tx", "U"]) fs.mkdirSync(path.join(D, d));
for (const f of ["T/f.txt", "T/sub/f.txt", "Tx/f.txt", "U/f.txt", "Tf.txt"]) fs.writeFileSync(path.join(D, f), "f");

const child = path.join(C, "child.cjs");
fs.writeFileSync(
  child,
  `const fs = require("fs");
const D = ${JSON.stringify(D)};
const out = [];
const row = (name, f) => {
  try {
    f();
    out.push(name + ":OK");
  } catch (e) {
    out.push(name + ":" + (e.code === "ERR_ACCESS_DENIED" ? "DENIED" : e.code));
  }
};
row("stat D", () => fs.statSync(D));
row("stat T", () => fs.statSync(D + "/T"));
row("readdir T", () => fs.readdirSync(D + "/T"));
row("read T/f", () => fs.readFileSync(D + "/T/f.txt"));
row("read T/sub/f", () => fs.readFileSync(D + "/T/sub/f.txt"));
row("read Tx/f", () => fs.readFileSync(D + "/Tx/f.txt"));
row("read Tf", () => fs.readFileSync(D + "/Tf.txt"));
row("read U/f", () => fs.readFileSync(D + "/U/f.txt"));
console.log("  " + out.join(" "));
out.length = 0;
row("write T/w", () => fs.writeFileSync(D + "/T/w.txt", "w"));
row("write T/sub/w", () => fs.writeFileSync(D + "/T/sub/w.txt", "w"));
row("write Tx/w", () => fs.writeFileSync(D + "/Tx/w.txt", "w"));
row("write U/w", () => fs.writeFileSync(D + "/U/w.txt", "w"));
row("mkdir T/n", () => fs.mkdirSync(D + "/T/n"));
console.log("  " + out.join(" "));
`,
);

const run = (label, grants) => {
  for (const f of ["T/w.txt", "T/sub/w.txt", "Tx/w.txt", "U/w.txt"]) fs.rmSync(path.join(D, f), { force: true });
  fs.rmSync(path.join(D, "T/n"), { recursive: true, force: true });
  const r = spawnSync(process.execPath, ["--permission", "--allow-fs-read=" + C, ...grants, child], {
    encoding: "utf8",
  });
  console.log("== " + label);
  console.log((r.stdout + r.stderr).split(D).join("<D>").trimEnd());
  console.log("  exit " + r.status);
};
const both = (entry) => ["--allow-fs-read=" + entry, "--allow-fs-write=" + entry];

run("dir/*", both(D + "/T/*"));
run("dir\\*", both(D + "\\T\\*"));
run("dir*", both(D + "/T*"));
run("dir/**", both(D + "/T/**"));
run("dir/*/f.txt", both(D + "/T/*/f.txt"));
run("dir/ (trailing separator, no wildcard)", both(D + "/T/"));
run("dir (no wildcard)", both(D + "/T"));
run("parent/*, read only", ["--allow-fs-read=" + D + "/*"]);
run("dir/* read, dir write", ["--allow-fs-read=" + D + "/T/*", "--allow-fs-write=" + D + "/T"]);
if (process.platform === "win32") {
  run("drive root \\*", ["--allow-fs-read=" + D.slice(0, 2) + "\\*"]);
  run("rooted \\*", ["--allow-fs-read=\\*"]);
  run("drive root /*", ["--allow-fs-read=" + D.slice(0, 2) + "/*"]);
} else {
  run("root /*", ["--allow-fs-read=/*"]);
}
