// fs.copyFile's mode, readdir's `recursive` option and fs.watch of a path
// that is not there, in the sync, callback and promise forms.
//
// Regression guard: copyFile ignored its mode, so COPYFILE_EXCL overwrote an
// existing destination and a mode outside 0..7 or of the wrong type was
// accepted; node v22.22.2 fails EEXIST `copyfile`, and refuses the mode
// (ERR_OUT_OF_RANGE / ERR_INVALID_ARG_TYPE) before touching a path. On
// Windows COPYFILE_FICLONE_FORCE is ENOSYS (libuv's copy cannot clone).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const start = process.cwd();
const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-378-"));
process.chdir(made);
const D = process.cwd();
const scrub = (s) => (typeof s === "string" ? s.split(D).join("<D>").split(made).join("<D>") : s);
const describe = (e) =>
  [e.code, e.name, e.errno, e.syscall, scrub(e.path), scrub(e.dest), scrub(e.message)].join(" | ");
const show = (label, f) => {
  try {
    const r = f();
    console.log(label + ": OK" + (r === undefined ? "" : " " + JSON.stringify(scrub(r))));
  } catch (e) {
    console.log(label + ": " + describe(e));
  }
};
const settle = async (label, f) => {
  try {
    const r = await f();
    console.log(label + ": OK" + (r === undefined ? "" : " " + JSON.stringify(scrub(r))));
  } catch (e) {
    console.log(label + ": " + describe(e));
  }
};
const viaCallback = (fn, ...args) => new Promise((res, rej) => fn(...args, (e, v) => (e ? rej(e) : res(v))));

fs.writeFileSync("f.txt", "f");
const { COPYFILE_EXCL, COPYFILE_FICLONE, COPYFILE_FICLONE_FORCE } = fs.constants;

// copyFile's mode.
const reset = () => fs.writeFileSync("e.txt", "e");
const modes = [undefined, null, 0, COPYFILE_EXCL, 1.5, -0.5, COPYFILE_FICLONE, 3, 7.9, 8, -1, NaN, Infinity, "1", true, 2 ** 32 + 1];
for (const mode of modes) {
  const label = "mode " + (typeof mode === "string" ? JSON.stringify(mode) : String(mode));
  reset();
  show("copyFileSync " + label, () => {
    fs.copyFileSync("f.txt", "e.txt", mode);
    return fs.readFileSync("e.txt", "utf8");
  });
  reset();
  await settle("copyFile " + label, async () => {
    await viaCallback(fs.copyFile, "f.txt", "e.txt", mode);
    return fs.readFileSync("e.txt", "utf8");
  });
  reset();
  await settle("promises.copyFile " + label, async () => {
    await fs.promises.copyFile("f.txt", "e.txt", mode);
    return fs.readFileSync("e.txt", "utf8");
  });
}
show("copyFileSync EXCL to a new file", () => {
  fs.copyFileSync("f.txt", "n1.txt", COPYFILE_EXCL);
  return fs.readFileSync("n1.txt", "utf8");
});
show("copyFileSync EXCL from a missing file", () => fs.copyFileSync("nope.txt", "n2.txt", COPYFILE_EXCL));
show("copyFileSync EXCL from a missing file made nothing", () => fs.existsSync("n2.txt"));
show("copyFileSync EXCL from a missing file onto one", () => fs.copyFileSync("nope.txt", "f.txt", COPYFILE_EXCL));
show("copyFileSync bad mode on a missing file", () => fs.copyFileSync("nope.txt", "n3.txt", 8));
if (process.platform === "win32") {
  show("copyFileSync FICLONE_FORCE", () => fs.copyFileSync("f.txt", "n4.txt", COPYFILE_FICLONE_FORCE));
  show("copyFileSync FICLONE_FORCE made nothing", () => fs.existsSync("n4.txt"));
} else {
  console.log("FICLONE_FORCE rows are Windows only");
}

// readdir with `recursive`: every entry below, names relative to the base,
// Dirents with the directory they were read from. readdirSync and readdir
// go breadth first; fs.promises.readdir pops its queue, so it lists the last
// directory found first; a link to a directory is followed except by a
// promise Dirent walk. oam used to list the top level only.
fs.mkdirSync("tree");
process.chdir("tree");
for (const d of ["a", "a/x", "a/x/deep", "b", "b/y", "c"]) fs.mkdirSync(d);
for (const f of ["top.txt", "a/1.txt", "a/x/2.txt", "a/x/deep/3.txt", "b/4.txt", "b/y/5.txt"]) fs.writeFileSync(f, f);
let link;
try {
  fs.symlinkSync(path.join(process.cwd(), "b"), "lnk", "dir");
  link = "made";
} catch (e) {
  link = e.code;
}
console.log("readdir link: " + link);
const dirents = (list) => list.map((d) => [d.name, scrub(d.parentPath), d.isDirectory()].join(":"));
show("readdirSync recursive", () => fs.readdirSync(".", { recursive: true }));
show("readdirSync recursive below", () => fs.readdirSync("a", { recursive: true }));
show("readdirSync recursive absolute", () => fs.readdirSync(process.cwd(), { recursive: true }));
show("readdirSync recursive withFileTypes", () =>
  dirents(fs.readdirSync(".", { recursive: true, withFileTypes: true })),
);
show("readdirSync recursive false", () => fs.readdirSync("a", { recursive: false }));
await settle("readdir recursive", () => viaCallback(fs.readdir, ".", { recursive: true }));
await settle("readdir recursive withFileTypes", async () =>
  dirents(await viaCallback(fs.readdir, ".", { recursive: true, withFileTypes: true })),
);
await settle("promises.readdir recursive", () => fs.promises.readdir(".", { recursive: true }));
await settle("promises.readdir recursive withFileTypes", async () =>
  dirents(await fs.promises.readdir(".", { recursive: true, withFileTypes: true })),
);
show("readdirSync recursive missing", () => fs.readdirSync("nope", { recursive: true }));
show("readdirSync recursive of a file", () => fs.readdirSync("top.txt", { recursive: true }));
await settle("promises.readdir recursive missing", () => fs.promises.readdir("nope", { recursive: true }));
await settle("readdir recursive missing", () => viaCallback(fs.readdir, "nope", { recursive: true }));
process.chdir(D);

// fs.watch of a path that is not there throws at the call, as libuv's
// `watch` error naming the path as given. oam used to hand back a watcher
// that never fired.
const watchError = (e) => [describe(e), Object.keys(e).join(","), scrub(e.filename)].join(" | ");
for (const p of ["nope", path.join(D, "nope"), path.join("f.txt", "sub")]) {
  try {
    fs.watch(p).close();
    console.log("watch " + JSON.stringify(scrub(p)) + ": watcher");
  } catch (e) {
    console.log("watch " + JSON.stringify(scrub(p)) + ": " + watchError(e));
  }
}
{
  const w = fs.watch("f.txt");
  w.close();
  console.log("watch f.txt: watcher");
}

process.chdir(start);
fs.rmSync(made, { recursive: true, force: true });
