// fs path arguments and the paths fs errors name (#167).
//
// - A path that is not a string, Buffer or URL is node's TypeError
//   ERR_INVALID_ARG_TYPE, naming the argument ("path", "oldPath", "src",
//   "prefix", ...), and one holding a NUL byte is ERR_INVALID_ARG_VALUE. oam
//   stringified it and asked the OS: `fs.statSync(42n)` stat'ed a file named
//   `42`, and `fs.writeFileSync(fd, data)` CREATED a file named after the
//   descriptor instead of writing to it.
// - readFile / writeFile / appendFile take an int32 as a file descriptor, and
//   fs/promises' take a FileHandle; truncate takes a descriptor (DEP0081).
// - On Windows an fs error names the RESOLVED path, as node's binding does
//   (`open 'C:\dir\missing.txt'` for `missing.txt`, `mkdir 'C:\dir\a\b'` for
//   `a/b`). Elsewhere the path is reported as passed; this case prints both
//   through the same `<root>` substitution, so it holds on every platform.
// - rename / copyFile / link name both paths (`'a' -> 'b'`) and set `dest`;
//   rmdir reports `rmdir`, not the lstat oam probes with; realpathSync and
//   fs.realpath report the lstat of the first missing component, as node's
//   JS walk does, while realpathSync.native and fs/promises.realpath report
//   `realpath`.
//
// Not covered: symlinks (creating one needs a privilege Windows hosts may not
// grant).
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const root = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), "oam-fspath-")));
process.chdir(root);
const clean = (value) => (typeof value === "string" ? value.split(root).join("<root>") : value);

function shape(err) {
  const out = { name: err.constructor.name, code: err.code };
  for (const key of ["errno", "syscall", "path", "dest"]) {
    if (key in err) out[key] = clean(err[key]);
  }
  out.message = clean(err.message);
  out.keys = Object.keys(err);
  return JSON.stringify(out);
}
function sync(label, fn) {
  try {
    const value = fn();
    console.log(label, "ok", typeof value === "string" ? JSON.stringify(clean(value)) : "");
  } catch (err) {
    console.log(label, shape(err));
  }
}
async function later(label, fn) {
  try {
    await fn();
    console.log(label, "ok");
  } catch (err) {
    console.log(label, shape(err));
  }
}
const viaCallback = (label, run) =>
  new Promise((resolve) => {
    try {
      run((err, value) => {
        console.log(label, "cb", err ? shape(err) : `ok ${typeof value === "string" ? JSON.stringify(clean(value)) : ""}`);
        resolve();
      });
    } catch (err) {
      console.log(label, "throws", shape(err));
      resolve();
    }
  });

console.log("-- argument types");
const bad = { bigint: 42n, object: {}, number: 42, null: null, undefined, boolean: true, nul: "a\0b" };
for (const [kind, value] of Object.entries(bad)) {
  sync(`statSync(${kind})`, () => fs.statSync(value));
  await later(`promises.readFile(${kind})`, () => fsp.readFile(value));
  await viaCallback(`stat(${kind})`, (cb) => fs.stat(value, cb));
}
sync("statSync(Buffer with NUL)", () => fs.statSync(Buffer.from([97, 0, 98])));
sync("statSync(legacy url.parse object)", () => fs.statSync({ protocol: "file:", href: "file:///x", path: "/x" }));
sync("renameSync(42, x)", () => fs.renameSync(42, "x"));
sync("renameSync(x, 42)", () => fs.renameSync("x", 42));
sync("copyFileSync(42, x)", () => fs.copyFileSync(42, "x"));
sync("copyFileSync(x, {})", () => fs.copyFileSync("x", {}));
sync("linkSync(42, x)", () => fs.linkSync(42, "x"));
sync("symlinkSync(42, x)", () => fs.symlinkSync(42, "x"));
sync("mkdtempSync(42)", () => fs.mkdtempSync(42));
sync("cpSync(x, 42)", () => fs.cpSync("x", 42));
sync("realpathSync.native(42)", () => fs.realpathSync.native(42));
await later("promises.realpath(42)", () => fsp.realpath(42));
await viaCallback("rm(42)", (cb) => fs.rm(42, cb));
await viaCallback("opendir(42)", (cb) => fs.opendir(42, cb));
console.log("existsSync(42)", fs.existsSync(42), "existsSync(NUL)", fs.existsSync("a\0b"));
console.log("no file named 42:", !fs.readdirSync(".").includes("42"));

console.log("-- descriptors and FileHandles");
fs.writeFileSync("data.txt", "hello");
const fd = fs.openSync("data.txt", "r+");
sync("readFileSync(fd)", () => fs.readFileSync(fd, "utf8"));
sync("writeFileSync(fd)", () => fs.writeFileSync(fd, " world"));
sync("appendFileSync(fd)", () => fs.appendFileSync(fd, "!"));
console.log("file now", JSON.stringify(fs.readFileSync("data.txt", "utf8")));
sync("readFileSync(-1)", () => fs.readFileSync(-1));
sync("readFileSync(1.5)", () => fs.readFileSync(1.5));
fs.closeSync(fd);
console.log("entries", JSON.stringify(fs.readdirSync(".").sort()));
const handle = await fsp.open("data.txt");
console.log("promises.readFile(handle)", JSON.stringify(String(await fsp.readFile(handle))));
await handle.close();

console.log("-- error paths");
sync("readFileSync", () => fs.readFileSync("missing.txt"));
sync("statSync", () => fs.statSync("missing.txt"));
sync("lstatSync", () => fs.lstatSync("missing.txt"));
sync("openSync", () => fs.openSync("missing.txt"));
sync("openSync wx", () => fs.openSync("data.txt", "wx"));
sync("accessSync", () => fs.accessSync("missing.txt"));
sync("readdirSync", () => fs.readdirSync("missing"));
sync("readdirSync(file)", () => fs.readdirSync("data.txt"));
sync("unlinkSync", () => fs.unlinkSync("missing.txt"));
sync("mkdirSync(nested)", () => fs.mkdirSync("nope/sub"));
sync("mkdirSync(existing)", () => fs.mkdirSync("."));
sync("statSync(a/../missing)", () => fs.statSync("sub/../missing"));
sync("statSync(empty)", () => fs.statSync(""));
sync("readlinkSync", () => fs.readlinkSync("missing"));
sync("chmodSync", () => fs.chmodSync("missing", 0o644));
sync("utimesSync", () => fs.utimesSync("missing", 1, 1));
sync("writeFileSync(nested)", () => fs.writeFileSync("nope/x.txt", "x"));
sync("renameSync", () => fs.renameSync("missing.txt", "x.txt"));
sync("copyFileSync", () => fs.copyFileSync("missing.txt", "x.txt"));
sync("linkSync", () => fs.linkSync("missing.txt", "y.txt"));
sync("rmdirSync(missing)", () => fs.rmdirSync("missing"));
sync("rmdirSync(file)", () => fs.rmdirSync("data.txt"));
sync("realpathSync", () => fs.realpathSync("missing"));
sync("realpathSync(nested)", () => fs.realpathSync("nope/sub/x"));
sync("realpathSync(file/x)", () => fs.realpathSync("data.txt/x"));
sync("realpathSync(ok)", () => fs.realpathSync("data.txt"));
sync("realpathSync.native", () => fs.realpathSync.native("missing"));
await later("promises.readFile", () => fsp.readFile("missing.txt"));
await later("promises.mkdir", () => fsp.mkdir("nope/sub"));
await later("promises.access", () => fsp.access("missing"));
await later("promises.rename", () => fsp.rename("missing.txt", "x.txt"));
await later("promises.copyFile", () => fsp.copyFile("missing.txt", "x.txt"));
await later("promises.link", () => fsp.link("missing.txt", "y.txt"));
await later("promises.rmdir", () => fsp.rmdir("missing"));
await later("promises.realpath", () => fsp.realpath("missing"));
await viaCallback("readFile", (cb) => fs.readFile("missing.txt", cb));
await viaCallback("rename", (cb) => fs.rename("missing.txt", "x.txt", cb));
await viaCallback("copyFile", (cb) => fs.copyFile("missing.txt", "x.txt", cb));
await viaCallback("rmdir", (cb) => fs.rmdir("missing", cb));
await viaCallback("realpath", (cb) => fs.realpath("nope/sub", cb));
await viaCallback("realpath.native", (cb) => fs.realpath.native("nope/sub", cb));
await new Promise((resolve) =>
  fs.createReadStream("missing.txt").on("error", (err) => {
    console.log("createReadStream", shape(err));
    resolve();
  }),
);

// fs.promises is the object require("fs/promises") returns, so a bad path
// REJECTS: it used to be the unwrapped methods, which threw before any
// promise existed, so `fs.promises.stat(bad).catch(...)` never attached.
console.log("fs.promises === fs/promises:", fs.promises === fsp);
for (const [name, args] of [
  ["stat", [42n]],
  ["readFile", [42n]],
  ["access", [42n]],
  ["unlink", [42n]],
  ["mkdir", [42n]],
  ["readdir", [42n]],
  ["writeFile", [42n, "x"]],
  ["appendFile", [42n, "x"]],
  ["rename", [42n, "b"]],
  ["copyFile", [42n, "b"]],
  ["open", [42n]],
  ["rm", [42n]],
]) {
  let settled;
  try {
    const p = fs.promises[name](...args);
    settled = p.then(
      () => `${name}: returned a promise, resolved`,
      (err) => `${name}: returned a promise, rejected ${shape(err)}`,
    );
  } catch (err) {
    settled = `${name}: threw synchronously ${shape(err)}`;
  }
  console.log(await settled);
}

process.chdir(os.tmpdir());
fs.rmSync(root, { recursive: true, force: true });
