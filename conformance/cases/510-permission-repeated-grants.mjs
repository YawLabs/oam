// A permission flag given more than once grants every value it was given
// (#363): `--allow-fs-write=A --allow-fs-write=B` admits writes under A and
// under B, and so does `--allow-fs-read`; a value in NODE_OPTIONS adds to the
// command line's; process.execArgv lists every occurrence, so a child started
// under `--permission` inherits all of them. A refusal's message names the
// flag that grants it: `Access to this API has been restricted. Use
// --allow-fs-write to manage permissions.`
//
// Regression guard: oam kept only the last occurrence of each list flag (an
// earlier grant was dropped, never added), and its denial message stopped at
// `restricted`.
//
// Not covered here: a value with a comma in it (node v22 takes `A,B` as one
// path and warns; oam splits it, docs/node-divergences.md section 4), and
// node's SecurityWarning for `--allow-child-process`, filtered below.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";

const D = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-510-")));
process.on("exit", () => fs.rmSync(D, { recursive: true, force: true }));
for (const d of ["A", "B", "C"]) {
  fs.mkdirSync(path.join(D, d));
  fs.writeFileSync(path.join(D, d, "f.txt"), d);
}
// NODE_OPTIONS splits on spaces and reads `\` inside quotes as an escape, so
// the values it carries are spelt with `/` (a Windows API takes either).
const fwd = (p) => p.split(path.sep).join("/");
const scrub = (s) =>
  s
    .split(D)
    .join("<D>")
    .split(fwd(D))
    .join("<D>")
    .replace(/\((node|oam):\d+\)/g, "(PID)")
    .replace(/^.*--trace-warnings.*$/gm, "")
    .replace(/^.*SecurityWarning: The flag --allow-child-process .*$/gm, "")
    .replace(/\\/g, "/");

const child = path.join(D, "child.cjs");
fs.writeFileSync(
  child,
  `const fs = require("fs");
const path = require("path");
const D = ${JSON.stringify(D)};
const row = (name, f) => {
  try {
    f();
    console.log(name + ": OK");
  } catch (e) {
    console.log(name + ": " + e.code + (e.permission ? " " + e.permission : "") + " " + JSON.stringify(e.message));
  }
};
for (const d of ["A", "B", "C"]) row("read " + d, () => fs.readFileSync(path.join(D, d, "f.txt")));
for (const d of ["A", "B", "C"]) row("write " + d, () => fs.writeFileSync(path.join(D, d, "w.txt"), "w"));
// The mode is an argument, not an environment variable: oam's --permission
// gates process.env too (docs/node-divergences.md, section 4).
const mode = process.argv[2];
if (mode === "spawn") {
  const { spawnSync } = require("child_process");
  const r = spawnSync(process.execPath, [__filename, "grandchild"], { encoding: "utf8" });
  console.log("grandchild:");
  console.log(r.stdout.trimEnd());
} else if (mode === undefined) {
  row("spawn", () => require("child_process").spawnSync(process.execPath, ["-e", "1"]));
  row("worker", () => new (require("worker_threads").Worker)(__filename).terminate());
}
console.log("execArgv " + JSON.stringify(process.execArgv.map((a) => a.split(D).join("<D>"))));
`,
);

const run = (label, args, nodeOptions, mode) => {
  for (const d of ["A", "B", "C"]) fs.rmSync(path.join(D, d, "w.txt"), { force: true });
  const env = { ...process.env };
  delete env.NODE_OPTIONS;
  if (nodeOptions) env.NODE_OPTIONS = nodeOptions;
  const childArgs = mode ? [child, mode] : [child];
  const r = spawnSync(process.execPath, ["--permission", ...args, ...childArgs], { encoding: "utf8", env });
  console.log("== " + label);
  for (const line of scrub(r.stdout + r.stderr).split("\n")) {
    if (line.trim() !== "") console.log("  " + line.trimEnd());
  }
  console.log("  exit " + r.status);
};

const A = path.join(D, "A");
const B = path.join(D, "B");
const readAll = "--allow-fs-read=" + D;

run("write granted twice", [readAll, "--allow-fs-write=" + A, "--allow-fs-write=" + B]);
run("read granted twice", ["--allow-fs-read=" + child, "--allow-fs-read=" + A, "--allow-fs-read=" + B]);
run("write granted three times", [
  readAll,
  "--allow-fs-write=" + A,
  "--allow-fs-write=" + path.join(D, "nowhere"),
  "--allow-fs-write=" + B,
]);
run("NODE_OPTIONS adds to argv", [readAll, "--allow-fs-write=" + A], "--allow-fs-write=" + fwd(B));
run(
  "NODE_OPTIONS twice",
  [readAll],
  "--allow-fs-write=" + fwd(A) + " --allow-fs-write=" + fwd(B),
);
run("a wildcard read beside a path", ["--allow-fs-read=" + A, "--allow-fs-read=*"]);
run(
  "a child inherits every grant",
  [readAll, "--allow-fs-write=" + A, "--allow-fs-write=" + B, "--allow-child-process"],
  undefined,
  "spawn",
);
