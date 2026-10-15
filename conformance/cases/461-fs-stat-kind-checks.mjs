// The fs calls that look at a stat before they act, as node's do: rm and
// rmdir check what the path is (a directory without `recursive`, a file
// under rmdir), cp checks whether source and destination are one file,
// realpath walks a path that does not resolve to say where it stops, and
// readdir's `recursive` and glob descend only into what is a directory.
// Each in its sync, callback and promise forms where it has them.
//
// Regression guard: these read the kind, identity or size off oam's stat
// record directly instead of building a Stats, and the record is shared by
// every sync stat; a misread field turns into the wrong error, or none.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-461-")));
const norm = (s) => String(s).split(root).join("<R>").replace(/\\/g, "/");
const errStr = (e) => `${e.name} ${e.code} ${e.syscall ?? ""} | ${norm(e.message)}`;
const show = (label, fn) => {
  try {
    const v = fn();
    console.log(label, "->", norm(v));
  } catch (e) {
    console.log(label, "THROWS", errStr(e));
  }
};
const showAsync = async (label, fn) => {
  try {
    console.log(label, "->", norm(await fn()));
  } catch (e) {
    console.log(label, "REJECTS", errStr(e));
  }
};
const viaCallback = (start) => () => new Promise((resolve, reject) => start((e, v) => (e ? reject(e) : resolve(v))));

const file = path.join(root, "f.txt");
const dir = path.join(root, "d");
const fresh = () => {
  fs.rmSync(file, { force: true });
  fs.rmSync(dir, { recursive: true, force: true });
  fs.writeFileSync(file, "hello");
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, "in.txt"), "x");
};

try {
  // ---- rm / rmdir: what the path is
  fresh();
  show("rmSync dir", () => fs.rmSync(dir));
  await showAsync("rm dir", viaCallback((cb) => fs.rm(dir, cb)));
  await showAsync("promises.rm dir", () => fs.promises.rm(dir));
  show("rmdirSync file", () => fs.rmdirSync(file));
  await showAsync("rmdir file", viaCallback((cb) => fs.rmdir(file, cb)));
  await showAsync("promises.rmdir file", () => fs.promises.rmdir(file));
  show("rmSync missing", () => fs.rmSync(path.join(root, "nope")));
  show("rmSync missing, force", () => fs.rmSync(path.join(root, "nope"), { force: true }));
  console.log("still there:", fs.existsSync(dir), fs.existsSync(file));
  show("rmSync file", () => fs.rmSync(file));
  show("rmSync dir, recursive", () => fs.rmSync(dir, { recursive: true }));
  console.log("gone:", fs.existsSync(dir), fs.existsSync(file));
  fresh();
  await showAsync("promises.rm dir, recursive", () => fs.promises.rm(dir, { recursive: true }));
  fs.mkdirSync(dir);
  await showAsync("promises.rmdir dir", () => fs.promises.rmdir(dir));
  console.log("gone:", fs.existsSync(dir));

  // ---- cp: one file or two
  fresh();
  const link = path.join(root, "hard.txt");
  fs.linkSync(file, link);
  show("cpSync file to itself", () => fs.cpSync(file, file));
  show("cpSync file to its hard link", () => fs.cpSync(file, link));
  show("cpSync dir into itself", () => fs.cpSync(dir, path.join(dir, "inner"), { recursive: true }));
  await showAsync("promises.cp file to itself", () => fs.promises.cp(file, file));
  show("cpSync file to a new name", () => fs.cpSync(file, path.join(root, "copy.txt")));
  console.log("copied:", fs.readFileSync(path.join(root, "copy.txt"), "utf8"));
  fs.rmSync(link);

  // ---- realpath of what does not resolve: where the walk stops
  show("realpathSync missing leaf", () => fs.realpathSync(path.join(dir, "nope", "x")));
  show("realpathSync under a file", () => fs.realpathSync(path.join(file, "x")));
  await showAsync("promises.realpath missing leaf", () => fs.promises.realpath(path.join(dir, "nope")));
  await showAsync("realpath under a file", viaCallback((cb) => fs.realpath(path.join(file, "x"), cb)));
  show("realpathSync dir", () => fs.realpathSync(dir));

  // ---- recursive readdir and glob descend into directories only
  fs.mkdirSync(path.join(dir, "a", "b"), { recursive: true });
  fs.writeFileSync(path.join(dir, "a", "b", "deep.txt"), "x");
  fs.writeFileSync(path.join(dir, "a.txt"), "x");
  show("readdirSync recursive", () => fs.readdirSync(dir, { recursive: true }).sort().join(" "));
  await showAsync("promises.readdir recursive", async () => (await fs.promises.readdir(dir, { recursive: true })).sort().join(" "));
  show("globSync **", () => fs.globSync("**", { cwd: dir }).map(norm).sort().join(" "));
  show("globSync */ nodir", () => fs.globSync("*", { cwd: dir, withFileTypes: true }).map((d) => `${d.name}:${d.isDirectory()}`).sort().join(" "));
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
