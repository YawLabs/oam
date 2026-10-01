// fs.close's result for a descriptor that is not open, and fs.rm's checks.
//
// node reports EBADF to fs.close's callback for a descriptor that was never
// opened or is already closed, the error closeSync throws; with no callback
// its default one throws it, an uncaught exception. oam called back null for
// both (fs.close went through the streams' forgiving close) and swallowed
// the failure when no callback was passed.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const shape = (e) =>
  `${e.constructor.name} ${e.code} ${e.syscall} ${JSON.stringify(e.message)} ${JSON.stringify(Object.keys(e))}`;
const uncaught = [];
process.on("uncaughtException", (e) => uncaught.push(shape(e)));

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-259-"));
const file = path.join(dir, "f.txt");
fs.writeFileSync(file, "x");

function close(label, fd) {
  return new Promise((resolve) => {
    let sync = true;
    fs.close(fd, (...args) => {
      console.log(label, sync ? "sync" : "async", args.length, args[0] ? shape(args[0]) : args[0]);
      resolve();
    });
    sync = false;
  });
}

await close("close(never opened, cb)", 987654);
const fd = fs.openSync(file, "r");
await close("close(fd, cb)", fd);
await close("close(fd, cb) again", fd);
try {
  fs.closeSync(fd);
} catch (e) {
  console.log("closeSync(fd) again", shape(e));
}
console.log("close.length", fs.close.length, fs.close.name);

// No callback: success is silent, a failure is an uncaught exception.
fs.close(fs.openSync(file, "r"));
fs.close(987654);
console.log("returned", fs.close(fs.openSync(file, "r")));
await new Promise((resolve) => setTimeout(resolve, 20));
console.log("uncaught", uncaught.length, uncaught.join(" | "));

// fs.rm never checks its callback (lib/fs.js, v22.22.2): the path and the
// options are validated synchronously, the removal runs, and calling the
// missing callback when it settles is an uncaught TypeError. oam refused
// the call with ERR_INVALID_ARG_TYPE for "cb" and removed nothing.
let onUncaught = null;
process.on("uncaughtException", () => onUncaught?.());
function rmWithout(label, run) {
  return new Promise((resolve) => {
    uncaught.length = 0;
    onUncaught = () => {
      onUncaught = null;
      console.log(label, "->", uncaught.join(" | "));
      resolve();
    };
    try {
      console.log(label, "returned", run());
    } catch (e) {
      onUncaught = null;
      console.log(label, "threw", shape(e));
      resolve();
    }
  });
}
const a = path.join(dir, "a");
const b = path.join(dir, "b");
const d = path.join(dir, "d");
fs.writeFileSync(a, "a");
fs.writeFileSync(b, "b");
fs.mkdirSync(d);
fs.writeFileSync(path.join(d, "x"), "x");
await rmWithout("rm(file)", () => fs.rm(a));
console.log("  removed", !fs.existsSync(a));
await rmWithout("rm(file, {}, 5)", () => fs.rm(b, {}, 5));
console.log("  removed", !fs.existsSync(b));
await rmWithout("rm(missing)", () => fs.rm(path.join(dir, "zz")));
await rmWithout("rm(missing, {force: true})", () => fs.rm(path.join(dir, "zz"), { force: true }));
await rmWithout("rm(dir)", () => fs.rm(d));
await rmWithout("rm(dir, {recursive: true})", () => fs.rm(d, { recursive: true }));
console.log("  removed", !fs.existsSync(d));
// Thrown at the call, with or without a callback.
for (const o of [null, "x", [], { recursive: 1 }, { force: "y" }, { retryDelay: -1 }, { retryDelay: "x" }, { maxRetries: 1.5 }]) {
  await rmWithout(`rm(p, ${JSON.stringify(o)})`, () => fs.rm(path.join(dir, "q"), o));
  await rmWithout(`rm(p, ${JSON.stringify(o)}, cb)`, () => fs.rm(path.join(dir, "q"), o, () => {}));
}
await rmWithout("rm(5)", () => fs.rm(5));
// rmSync and fs/promises.rm validate the same options the same way.
for (const o of [null, { recursive: 1 }, { maxRetries: -1 }]) {
  try {
    fs.rmSync(path.join(dir, "q"), o);
    console.log("rmSync no error");
  } catch (e) {
    console.log(`rmSync(p, ${JSON.stringify(o)})`, shape(e));
  }
  await fs.promises.rm(path.join(dir, "q"), o).then(
    () => console.log("promises.rm no error"),
    (e) => console.log(`promises.rm(p, ${JSON.stringify(o)})`, shape(e)),
  );
}
console.log("rmSync.length", fs.rmSync.length);
console.log("rm.length", fs.rm.length, fs.rm.name);
// With a callback: one argument, null on success.
fs.writeFileSync(a, "a");
await new Promise((resolve) => fs.rm(a, (...args) => { console.log("rm(file, cb)", args.length, args[0]); resolve(); }));
await new Promise((resolve) =>
  fs.rm(a, (...args) => { console.log("rm(missing, cb)", args.length, args[0].code); resolve(); }),
);

// A directory without `recursive` is ERR_FS_EISDIR, removed by none of the
// three forms (node's validateRmOptions lstats the path first); a path whose
// lstat fails reports the lstat, except ENOENT under `force`. oam used to
// remove an empty directory and fail a full one with ENOTEMPTY.
const where = (s) => String(s).split(dir).join("<dir>");
const rmShape = (e) =>
  `${e.constructor.name} ${e.name} ${e.code} ${e.errno} ${e.syscall} ${where(e.path)} ${JSON.stringify(where(e.message))} ${JSON.stringify(Object.keys(e))}` +
  (e.info ? ` info ${JSON.stringify({ ...e.info, path: where(e.info.path) })} ${where(String(e))}` : "");
const tree = path.join(dir, "tree");
const seed = () => {
  fs.rmSync(tree, { recursive: true, force: true });
  fs.mkdirSync(path.join(tree, "empty"), { recursive: true });
  fs.mkdirSync(path.join(tree, "full"));
  fs.writeFileSync(path.join(tree, "full", "a"), "");
  fs.writeFileSync(path.join(tree, "file"), "");
};
for (const [name, o] of [
  ["empty", undefined], ["full", undefined], ["empty", { force: true }], ["empty", { recursive: false }],
  ["full", { recursive: true }], ["missing", undefined], ["missing", { force: true }], ["file", undefined],
  ["missing/deeper", undefined], ["file/sub", { force: true }],
]) {
  const p = path.join(tree, name);
  const label = `${name} ${JSON.stringify(o)}`;
  seed();
  try {
    fs.rmSync(p, o);
    console.log("rmSync", label, "ok", fs.existsSync(p));
  } catch (e) {
    console.log("rmSync", label, rmShape(e), fs.existsSync(p));
  }
  seed();
  await new Promise((resolve) =>
    fs.rm(p, ...(o ? [o] : []), (...args) => {
      console.log("rm", label, args.length, args[0] ? rmShape(args[0]) : args[0], fs.existsSync(p));
      resolve();
    }),
  );
  seed();
  await fs.promises.rm(p, o).then(
    () => console.log("promises.rm", label, "ok", fs.existsSync(p)),
    (e) => console.log("promises.rm", label, rmShape(e), fs.existsSync(p)),
  );
}
seed();
try {
  fs.rmSync(path.join(tree, "empty"));
} catch (e) {
  e.errno = 5;
  console.log("errno is an accessor over info:", e.info.errno, e.errno);
}

fs.rmSync(dir, { recursive: true, force: true });
