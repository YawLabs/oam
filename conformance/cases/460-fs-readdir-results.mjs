// What every readdir form hands back for one tree: the names (non-ASCII
// ones too) in the order node gives them, the Dirents' kinds and parents,
// with and without `recursive`, an empty directory, and opendir's Dir read
// a few entries at a time. Each call's array is its own: a caller that
// changes it changes nothing a later call returns.
//
// Regression guard: oam's natives hand a listing over as two arrays, names
// and kinds, which plain readdir returns as they are and the Dirent forms
// walk together; a shared or reused names array, or kinds out of step with
// the names, would show here.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-460-")));
const norm = (s) => String(s).split(root).join("<R>").replace(/\\/g, "/");
const tree = path.join(root, "t");
fs.mkdirSync(tree);
for (const name of ["b.txt", "a.txt", "été.md", "日本.txt", "emoji \u{1F600}.txt", "Z.txt", "10.txt", "9.txt"]) {
  fs.writeFileSync(path.join(tree, name), name);
}
fs.mkdirSync(path.join(tree, "sub"));
fs.mkdirSync(path.join(tree, "sub", "deeper"));
fs.writeFileSync(path.join(tree, "sub", "inner.js"), "1");
fs.writeFileSync(path.join(tree, "sub", "deeper", "leaf.txt"), "1");
fs.mkdirSync(path.join(tree, "empty"));
const dirent = (d) => `${d.name}<${d.isFile() ? "f" : ""}${d.isDirectory() ? "d" : ""}${d.isSymbolicLink() ? "l" : ""}>@${norm(d.parentPath)}`;
const show = (label, value) => {
  const list = Array.isArray(value) ? value : [value];
  console.log(label, list.map((v) => (typeof v === "string" ? norm(v) : dirent(v))).join(" | "));
};

try {
  show("readdirSync:", fs.readdirSync(tree));
  show("withFileTypes:", fs.readdirSync(tree, { withFileTypes: true }));
  show("promises:", await fs.promises.readdir(tree));
  show("promises withFileTypes:", await fs.promises.readdir(tree, { withFileTypes: true }));
  show("callback:", await new Promise((r) => fs.readdir(tree, (e, v) => r(e ?? v))));
  show("callback withFileTypes:", await new Promise((r) => fs.readdir(tree, { withFileTypes: true }, (e, v) => r(e ?? v))));
  show("recursive sync:", fs.readdirSync(tree, { recursive: true }));
  show("recursive sync dirents:", fs.readdirSync(tree, { recursive: true, withFileTypes: true }));
  show("recursive promises:", await fs.promises.readdir(tree, { recursive: true }));
  show("recursive callback dirents:", await new Promise((r) => fs.readdir(tree, { recursive: true, withFileTypes: true }, (e, v) => r(e ?? v))));
  for (const form of ["sync", "promises"]) {
    const empty = path.join(tree, "empty");
    const v = form === "sync" ? fs.readdirSync(empty) : await fs.promises.readdir(empty);
    console.log(`empty ${form}:`, JSON.stringify(v), Array.isArray(v), Object.getPrototypeOf(v) === Array.prototype);
  }

  // opendir, two entries at a time, recursive.
  {
    const dir = fs.opendirSync(tree, { bufferSize: 2, recursive: true });
    const read = [];
    for (let d = dir.readSync(); d !== null; d = dir.readSync()) read.push(d);
    dir.closeSync();
    show("opendirSync:", read);
    const read2 = [];
    for await (const d of await fs.promises.opendir(path.join(tree, "sub"), { bufferSize: 1 })) read2.push(d);
    show("promises.opendir:", read2);
  }

  // Each call's array is its own.
  {
    const a = fs.readdirSync(tree);
    const b = fs.readdirSync(tree);
    const length = a.length;
    a.push("added");
    a[0] = "changed";
    const c = fs.readdirSync(tree);
    console.log("own arrays:", a !== b, b.length === length, c.length === length, c[0] === b[0], c.includes("added"));
    const p1 = await fs.promises.readdir(tree);
    p1.length = 0;
    const p2 = await fs.promises.readdir(tree);
    console.log("own promise arrays:", p2.length === length);
    const names = fs.readdirSync(tree);
    const dirents = fs.readdirSync(tree, { withFileTypes: true });
    console.log("dirents follow names:", dirents.map((d) => d.name).join("/") === names.join("/"));
  }

  // glob reads the same listings.
  console.log("globSync:", fs.globSync("**/*.{txt,js}", { cwd: tree }).map(norm).sort().join(" | "));
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
