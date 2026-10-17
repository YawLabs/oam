// fs.cp, fs.cpSync and fs.promises.cp make a missing destination parent,
// as node's do (node's checkParentDir, and CpSyncCheckPaths in the sync
// copy), and a parent that cannot be made fails as node's does (#360): the
// async forms with mkdir's ENOTDIR naming the file in the way, a file
// under a file with copyfile's ENOENT, and cpSync of a directory with the
// error of node's C++ std::filesystem::create_directories -- on Windows
// MSVC's (errno 183, no code, the system's message), elsewhere what the
// platform's lstat says first. fs.cp calls back (null, undefined).
//
// Regression guard: oam's copy failed ENOENT for a file whose destination
// parent was missing, swallowed the mkdir failure of a directory copy, and
// gave cpSync's std::filesystem errors the key order errno, code, syscall,
// path (node's ErrnoException sets path before syscall) and, on Windows,
// Linux's errno, code and text.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-532-")));
process.chdir(root);
const norm = (s) => {
  let out = String(s);
  for (const r of [path.toNamespacedPath(root), root]) out = out.split(r).join("<R>");
  return out.replace(/\\/g, "/");
};
fs.mkdirSync("src/sub", { recursive: true });
fs.writeFileSync("src/top.txt", "top");
fs.writeFileSync("src/sub/in.txt", "in");
fs.writeFileSync("blocker", "file");
const tree = (d) => {
  try {
    return fs.readdirSync(d, { recursive: true }).map((p) => p.split(path.sep).join("/")).sort().join(" ");
  } catch (e) {
    return e.code;
  }
};
const errStr = (e) =>
  `${e.constructor.name} ${JSON.stringify(e.code)} ${e.errno} ${e.syscall} keys=${Object.keys(e)} | ${norm(e.message)}` +
  (e.path !== undefined ? ` | path=${norm(e.path)}` : "") + (e.dest !== undefined ? ` | dest=${norm(e.dest)}` : "");
const forms = {
  cpSync: (s, d, o) => {
    try {
      return Promise.resolve(`returned ${fs.cpSync(s, d, o)}`);
    } catch (e) {
      return Promise.reject(e);
    }
  },
  "promises.cp": (s, d, o) => fs.promises.cp(s, d, o).then((v) => `resolved ${v}`),
  cp: (s, d, o) =>
    new Promise((resolve, reject) => {
      const done = (...args) => (args[0] ? reject(args[0]) : resolve(`called back ${args.length} ${args.map(String)}`));
      o === undefined ? fs.cp(s, d, done) : fs.cp(s, d, o, done);
    }),
};
const rows = [
  ["file, parents missing", "src/top.txt", (f) => `out-${f}/n1/n2/top.txt`, undefined, (f) => `out-${f}`],
  ["dir, parents missing", "src", (f) => `outd-${f}/n1/copy`, { recursive: true }, (f) => `outd-${f}`],
  ["file, a parent is a file", "src/top.txt", () => "blocker/x/top.txt"],
  ["file, the parent is a file", "src/top.txt", () => "blocker/top.txt"],
  ["dir, a parent is a file", "src", () => "blocker/x/copy", { recursive: true }],
  ["dir, the parent is a file", "src", () => "blocker/copy", { recursive: true }],
  ["dir, deeper under a file", "src", () => "blocker/x/y/z", { recursive: true }],
  ["dir with filter, a parent is a file", "src", () => "blocker/x/copy", { recursive: true, filter: () => true }],
];
for (const [name, form] of Object.entries(forms)) {
  for (const [label, src, dest, opts, inspect] of rows) {
    let out;
    try {
      out = await form(src, dest(name), opts);
    } catch (e) {
      out = errStr(e);
    }
    console.log(`${name} ${label}: ${out}` + (inspect ? ` | tree: ${tree(inspect(name))}` : ""));
  }
}

// A parent that cannot be made for want of permission (not on Windows,
// where a directory's mode does not stop one being made in it).
if (process.platform !== "win32") {
  fs.mkdirSync("ro");
  fs.chmodSync("ro", 0o555);
  for (const [name, form] of Object.entries(forms)) {
    let out;
    try {
      out = await form("src", "ro/x/copy", { recursive: true });
    } catch (e) {
      out = errStr(e);
    }
    console.log(`${name} dir under a read-only dir: ${out}`);
  }
  fs.chmodSync("ro", 0o755);
} else {
  console.log("read-only rows not run on win32");
}

process.chdir(os.tmpdir());
fs.rmSync(root, { recursive: true, force: true });
