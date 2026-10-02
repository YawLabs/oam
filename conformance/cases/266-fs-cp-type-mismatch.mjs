// fs.cpSync / fs.cp / fs.promises.cp of a directory onto something that is
// not a directory, and of a file onto a directory.
//
// Regression guard: oam made the directory, failed to, and went on -- a
// directory copied onto a file failed on its first entry with ENOENT
// (copyfile), or not at all when it was empty; a file copied onto a
// directory failed with EPERM from copyfile. Node v22.22.2 checks first
// (checkPaths, and cpSyncCheckPaths for cpSync): ERR_FS_CP_DIR_TO_NON_DIR
// and ERR_FS_CP_NON_DIR_TO_DIR, ahead of the `recursive` check, nothing
// copied. cpSync's error is a plain Error with `code` alone and the paths
// as node hands them to its C++ (path.toNamespacedPath); cp's carries
// `info`, `errno`, `syscall` and `path` and names the paths as given.
// (Its class, SystemError, oam does not have: docs/node-divergences.md,
// "Coded errors", so the error's name is not printed.)
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const base = fs.mkdtempSync(path.join(os.tmpdir(), "oam-cp-"));
process.chdir(base);
fs.mkdirSync("d");
fs.writeFileSync(path.join("d", "inner"), "x");
fs.mkdirSync("empty");
fs.writeFileSync("f", "file");
fs.mkdirSync("dd");
fs.mkdirSync(path.join("n", "sub"), { recursive: true });
fs.writeFileSync(path.join("n", "file"), "1");
fs.mkdirSync("m");
fs.writeFileSync(path.join("m", "sub"), "a file");
fs.mkdirSync(path.join("m", "file"));

const cwd = process.cwd();
const ns = path.toNamespacedPath(cwd);
const rel = (s) => String(s).split(ns).join("<cwd>").split(cwd).join("<cwd>");
const describe = (e) => {
  if (e === undefined) return "ok";
  const fields = [
    `code ${e.code}`,
    `message ${rel(e.message)}`,
    `keys ${Object.keys(e).join(",")}`,
    `plain Error ${Object.getPrototypeOf(e) === Error.prototype}`,
  ];
  if (e.info !== undefined) {
    fields.push(`info ${rel(JSON.stringify(e.info))}`, `errno ${e.errno}`, `syscall ${e.syscall}`, `path ${rel(e.path)}`);
  }
  return fields.join(" | ");
};
const state = () =>
  `f ${fs.statSync("f").isFile() ? fs.readFileSync("f", "utf8") : "dir"}, dd [${fs.readdirSync("dd")}], m [${fs.readdirSync("m")}]`;

for (const [label, src, dest, options] of [
  ["dir onto a file", "d", "f", { recursive: true }],
  ["empty dir onto a file", "empty", "f", { recursive: true }],
  ["dir onto a file, no recursive", "d", "f", {}],
  ["dir onto a file, force false", "d", "f", { recursive: true, force: false }],
  ["file onto a dir", "f", "dd", {}],
  ["file onto a dir, recursive", "f", "dd", { recursive: true }],
]) {
  let failure;
  try {
    fs.cpSync(src, dest, options);
  } catch (e) {
    failure = e;
  }
  console.log(`cpSync ${label}: ${describe(failure)} || ${state()}`);
  failure = undefined;
  try {
    await fs.promises.cp(src, dest, options);
  } catch (e) {
    failure = e;
  }
  console.log(`promises.cp ${label}: ${describe(failure)} || ${state()}`);
  failure = await new Promise((resolve) => fs.cp(src, dest, options, (e) => resolve(e ?? undefined)));
  console.log(`cp ${label}: ${describe(failure)} || ${state()}`);
}

// A mismatch below the top: node's cp checks every entry it copies (its
// cpSync copies the tree in C++, which fails there with the platform's own
// error -- not compared).
{
  let failure;
  try {
    await fs.promises.cp("n", "m", { recursive: true });
  } catch (e) {
    failure = e;
  }
  console.log(`promises.cp nested file onto a dir: ${describe(failure)} || ${state()}`);
}

process.chdir(os.tmpdir());
fs.rmSync(base, { recursive: true, force: true });
