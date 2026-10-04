// Windows fs paths are namespaced as node's binding namespaces them (#275):
// every path-taking op hands the OS path.toNamespacedPath(p) -- resolved
// against the real cwd and `\\?\`-prefixed -- so the OS applies no Win32
// normalisation: a trailing dot or space stays in the name, `NUL` / `COM1`
// are files, not devices, and `sub/` opens the directory and fails the read.
// Errors name that path with the prefix taken off and never resolved again
// (`mkdirSync("C:\\")` says 'C:\', not the volume 'C:'), a patched
// process.cwd changes nothing, a symlink's target is stored as node stores
// it, and --permission judges the namespaced path (a trailing dot is not
// the granted directory, a device spelling matches no grant).
//
// Regression guard: oam passed the path as given to std, so `.env.` read
// `.env`, `NUL` read the device, `writeFileSync("g.txt.")` created `g.txt`,
// and the permission check matched `allowed.\x` against a grant of `allowed`.
// The rows outside the Windows block hold on every platform.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";

const start = process.cwd();
const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-377-"));
process.chdir(made);
const D = process.cwd();
const realCwd = process.cwd;
const root = D.slice(0, 3);
const scrub = (s) => {
  if (typeof s !== "string") return s;
  // The drive letter as spelled, and lower-cased (a row asks for it).
  const lowered = D[0].toLowerCase() + D.slice(1);
  let out = s.split(D).join("<D>").split(made).join("<D>").split(lowered).join("<d>");
  if (process.platform === "win32") out = out.split(root).join("<ROOT>");
  return out;
};
const show = (label, f) => {
  let line;
  try {
    const r = f();
    line = "OK" + (r === undefined ? "" : " " + JSON.stringify(scrub(r)));
  } catch (e) {
    line = [e.code, e.syscall, "path=" + JSON.stringify(scrub(e.path)), scrub(e.message)].join(" ");
  }
  console.log(label + ": " + line);
};

fs.mkdirSync("sub");
fs.mkdirSync("d");
fs.mkdirSync("envdir");
fs.writeFileSync("d/a.txt", "a");
fs.writeFileSync(".env", "A=1\n");
fs.writeFileSync("file.txt", "f");

// Every platform: the name is the name.
for (const p of [".env.", ".env ", "NUL"]) {
  show("readFileSync " + JSON.stringify(p), () => fs.readFileSync(p, "utf8"));
}
show("statSync .env.", () => fs.statSync(".env.").size);
show("existsSync .env.", () => fs.existsSync(".env."));
show("openSync NUL", () => typeof fs.openSync("NUL", "r"));
show("loadEnvFile .env.", () => process.loadEnvFile(".env."));
show("writeFileSync g.txt.", () => {
  fs.writeFileSync("g.txt.", "g");
  return fs.readdirSync(".").filter((n) => n.startsWith("g")).sort();
});
show("readFileSync g.txt.", () => fs.readFileSync("g.txt.", "utf8"));
show("unlinkSync g.txt.", () => fs.unlinkSync("g.txt."));

if (process.platform !== "win32") {
  console.log("the Windows namespace rows are Windows only");
} else {
  for (const p of ["sub/", "sub\\", "nul", "f.txt..."]) {
    show("readFileSync " + JSON.stringify(p), () => fs.readFileSync(p, "utf8"));
  }
  show("statSync NUL", () => fs.statSync("NUL").size);
  show("statSync COM1", () => fs.statSync("COM1").size);
  show("loadEnvFile sub/", () => process.loadEnvFile("sub/"));
  show("loadEnvFile NUL", () => process.loadEnvFile("NUL"));

  // Drive roots: the namespaced root is `\\?\X:\`; resolving it again
  // gives the volume `\\?\X:`, so the error must not be re-resolved.
  show("mkdirSync root", () => fs.mkdirSync(root));
  show("mkdirSync root via ..", () => fs.mkdirSync(root + "x\\.."));
  // A drive or share root opens in node (FILE_FLAG_BACKUP_SEMANTICS): its
  // readFile fails on the read and every write open is EPERM. oam opens
  // without that flag, where a root fails not-found, and it read as ENOENT.
  // A read-only openSync of a root is not compared: node gets a directory
  // descriptor oam does not have (docs/node-divergences.md).
  const share = "\\\\localhost\\" + D[0] + "$";
  const roots = [root, "\\", "/", D.slice(0, 2) + "\\.."];
  if (fs.existsSync(share)) roots.push(share, share + "\\");
  for (const r of roots) {
    const label = JSON.stringify(r === share || r === share + "\\" ? r.replace(share, "<SHARE>") : scrub(r));
    show("readFileSync root " + label, () => fs.readFileSync(r));
    show("writeFileSync root " + label, () => fs.writeFileSync(r, ""));
    show("appendFileSync root " + label, () => fs.appendFileSync(r, ""));
    show("openSync root w " + label, () => fs.openSync(r, "w"));
    show("openSync root a " + label, () => fs.openSync(r, "a"));
    show("loadEnvFile root " + label, () => process.loadEnvFile(r));
  }

  // A patched process.cwd: node's binding resolves against the real one.
  process.cwd = () => "C:\\patched";
  show("patched readFileSync missing", () => fs.readFileSync("missing.txt"));
  show("patched rmdirSync file", () => fs.rmdirSync("file.txt"));
  show("patched cpSync dir onto file", () => fs.cpSync("d", "file.txt", { recursive: true }));
  show("patched cpSync dir no recursive", () => fs.cpSync("d", "d2"));
  show("patched loadEnvFile missing", () => process.loadEnvFile("missing.env"));
  show("patched loadEnvFile dir", () => process.loadEnvFile("envdir"));
  process.cwd = realCwd;

  // A symlink's target as node's preprocessSymlinkDestination stores it,
  // seen in the EEXIST message (the link already exists).
  const targets = [
    ["abs fwd", D.replaceAll("\\", "/") + "/sub/a.txt"],
    ["abs bs", D + "\\sub\\a.txt"],
    ["rel fwd", "sub/a.txt"],
    ["rooted", D.slice(2) + "\\sub"],
    ["verbatim", "\\\\?\\" + D + "\\sub"],
    ["drive root", root],
    ["rel dotdot", "../x/./y"],
  ];
  for (const [label, target] of targets) {
    show("symlinkSync EEXIST " + label, () => fs.symlinkSync(target, "file.txt"));
  }

  // --permission matching on the namespaced path, in a child.
  fs.mkdirSync("allowed");
  fs.writeFileSync("allowed/f.txt", "f");
  fs.writeFileSync(
    "allowed/child.cjs",
    `const fs = require("fs");
const A = __dirname;
const F = A + "\\\\f.txt";
const rows = [
  ["plain", F],
  ["lower drive", F[0].toLowerCase() + F.slice(1)],
  ["verbatim", "\\\\\\\\?\\\\" + F],
  ["device", "\\\\\\\\.\\\\" + F],
  ["rooted", F.slice(2)],
  ["fwd rooted", F.slice(2).replaceAll("\\\\", "/")],
  ["trailing dot dir", A + ".\\\\f.txt"],
  ["trailing space dir", A + " \\\\f.txt"],
  ["dir trailing sep", A + "\\\\"],
  ["dir dot", A + "."],
  ["other drive", "Q:\\\\f.txt"],
  ["verbatim other drive", "\\\\\\\\?\\\\Q:\\\\f.txt"],
  ["device other drive", "\\\\\\\\.\\\\Q:\\\\f.txt"],
  ["UNC", "\\\\\\\\srv\\\\sh\\\\x"],
  ["verbatim UNC", "\\\\\\\\?\\\\UNC\\\\srv\\\\sh\\\\x"],
  ["GLOBALROOT", "\\\\\\\\?\\\\GLOBALROOT\\\\??\\\\" + F],
];
for (const [name, p] of rows) {
  let r;
  try {
    fs.statSync(p);
    r = "OK";
  } catch (e) {
    r = e.code === "ERR_ACCESS_DENIED" ? "DENIED " + e.permission + " " + e.resource : "allowed " + e.code;
  }
  console.log(name + ": " + r);
}
`,
  );
  for (const grant of [path.join(D, "allowed"), "\\"]) {
    console.log("--permission --allow-fs-read=" + scrub(grant));
    const r = spawnSync(
      process.execPath,
      ["--permission", "--allow-fs-read=" + grant, path.join(D, "allowed", "child.cjs")],
      { encoding: "utf8" },
    );
    for (const line of (r.stdout + r.stderr).trimEnd().split("\n")) console.log("  " + scrub(line.trimEnd()));
    console.log("  exit " + r.status);
  }
}

process.chdir(start);
fs.rmSync(made, { recursive: true, force: true });
