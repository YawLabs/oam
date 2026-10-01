// The path forms of truncate, chmod, chown, lchown, utimes and lutimes check
// their other arguments as node's do, and where node does.
//
// node v22.22.2 (lib/fs.js, lib/internal/fs/promises.js):
// - truncate: a path is opened "r+" and its descriptor ftruncated, so a
//   missing file is ENOENT before the length is looked at; the length an
//   integer (a negative one 0), and the callback form checks the length, the
//   callback and the path at the call;
// - chmod: the path, then parseFileMode, then (callback form) the callback;
// - chown / lchown: (callback form) the callback, the path, uid, gid in
//   [-1, 2**32-1];
// - utimes / lutimes: (callback form) the callback, the path, the times under
//   toUnixTimestamp's default name, "time".
// oam checked none of these: truncateSync(f, 'x') emptied the file,
// chmodSync(f, 'zz') made it read-only, chown(f, -2, 0) succeeded, and the
// callback forms reported a bad path or argument through the callback.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-263-"));
const f = path.join(dir, "f");
const missing = path.join(dir, "missing");
process.removeAllListeners("warning");
process.on("warning", (w) => console.log("warning", w.code));

function shape(e) {
  return `${e.constructor.name} ${e.code}${e.syscall ? " " + e.syscall : ""} ${JSON.stringify(e.message.split(dir).join("<dir>"))}`;
}
const show = (v) =>
  typeof v === "bigint" ? `${v}n` : typeof v === "string" ? `'${v}'` : v instanceof Date ? "Date" : v !== null && typeof v === "object" ? "{}" : String(v);
const reset = () => {
  if (fs.existsSync(f)) fs.chmodSync(f, 0o666);
  fs.writeFileSync(f, "ABCDEF");
};
const state = () => `size ${fs.statSync(f).size}, ${fs.statSync(f).mode & 0o200 ? "writable" : "read-only"}`;
function sync(label, fn) {
  try {
    fn();
    console.log(label, "->", "ok");
  } catch (e) {
    console.log(label, "!!", shape(e));
  }
}
async function promised(label, fn) {
  try {
    await fn();
    console.log(label, "->", "ok");
  } catch (e) {
    console.log(label, "rejects", shape(e));
  }
}
function settle(label, fn) {
  return new Promise((resolve) => {
    try {
      fn((...args) => {
        console.log(label, "cb", args[0] ? shape(args[0]) : `${args[0]}, ${args.length} argument(s)`);
        resolve();
      });
    } catch (e) {
      console.log(label, "!!", shape(e));
      resolve();
    }
  });
}

for (const len of ["x", 1.5, null, -3, 2, undefined, 3n, NaN]) {
  reset();
  sync(`truncateSync(f, ${show(len)})`, () => fs.truncateSync(f, len));
  console.log("  ", state());
  reset();
  await settle(`truncate(f, ${show(len)}, cb)`, (cb) => fs.truncate(f, len, cb));
  console.log("  ", state());
  reset();
  await promised(`fsp.truncate(f, ${show(len)})`, () => fsp.truncate(f, len));
  console.log("  ", state());
  sync(`truncateSync(missing, ${show(len)})`, () => fs.truncateSync(missing, len));
  await settle(`truncate(missing, ${show(len)}, cb)`, (cb) => fs.truncate(missing, len, cb));
  await promised(`fsp.truncate(missing, ${show(len)})`, () => fsp.truncate(missing, len));
  sync(`truncateSync(12.5, ${show(len)})`, () => fs.truncateSync(12.5, len));
  sync(`truncate(12.5, ${show(len)}, cb)`, () => fs.truncate(12.5, len, () => {}));
}
reset();
await settle("truncate(f, cb)", (cb) => fs.truncate(f, cb));
console.log("  ", state());
sync("truncate(f, 'x') no callback", () => fs.truncate(f, "x"));
sync("truncate(f, 1) no callback", () => fs.truncate(f, 1));
sync("truncate(12.5, 1) no callback", () => fs.truncate(12.5, 1));

for (const m of ["zz", "9", -1, 1.5, undefined, null, "644"]) {
  reset();
  sync(`chmodSync(f, ${show(m)})`, () => fs.chmodSync(f, m));
  console.log("  ", state());
  reset();
  await settle(`chmod(f, ${show(m)}, cb)`, (cb) => fs.chmod(f, m, cb));
  console.log("  ", state());
  reset();
  await promised(`fsp.chmod(f, ${show(m)})`, () => fsp.chmod(f, m));
  console.log("  ", state());
  sync(`chmodSync(12.5, ${show(m)})`, () => fs.chmodSync(12.5, m));
  sync(`chmod(12.5, ${show(m)}, cb)`, () => fs.chmod(12.5, m, () => {}));
  await promised(`fsp.chmod(12.5, ${show(m)})`, () => fsp.chmod(12.5, m));
}
sync("chmod(f, 'zz') no callback", () => fs.chmod(f, "zz"));
sync("chmod(f, 0o666) no callback", () => fs.chmod(f, 0o666));
reset();

for (const [u, g] of [[-2, 0], [0, -2], [1.5, 0], ["x", 0], [undefined, 0], [2 ** 32, 0], [-1, -1]]) {
  for (const fn of ["chown", "lchown"]) {
    sync(`${fn}Sync(f, ${show(u)}, ${show(g)})`, () => fs[fn + "Sync"](f, u, g));
    await settle(`${fn}(f, ${show(u)}, ${show(g)}, cb)`, (cb) => fs[fn](f, u, g, cb));
    await promised(`fsp.${fn}(f, ${show(u)}, ${show(g)})`, () => fsp[fn](f, u, g));
    sync(`${fn}Sync(12.5, ${show(u)}, ${show(g)})`, () => fs[fn + "Sync"](12.5, u, g));
    sync(`${fn}(12.5, ${show(u)}, ${show(g)}, cb)`, () => fs[fn](12.5, u, g, () => {}));
  }
}
for (const fn of ["chown", "lchown", "utimes", "lutimes"]) {
  const ok = fn.endsWith("utimes") ? [1, 1] : [0, 0];
  sync(`${fn}(12.5) no callback`, () => fs[fn](12.5, ...ok));
  sync(`${fn}(f, 'x', {}) no callback`, () => fs[fn](f, "x", {}));
  sync(`${fn}(f, 'x', {}, cb)`, () => fs[fn](f, "x", {}, () => console.log("  called back")));
  sync(`${fn}(12.5, cb)`, () => fs[fn](12.5, ...ok, () => console.log("  called back")));
}
for (const [a, m] of [["x", 1], [1, "x"], [1, {}], [new Date(0), 2]]) {
  sync(`utimesSync(f, ${show(a)}, ${show(m)})`, () => fs.utimesSync(f, a, m));
  sync(`lutimesSync(f, ${show(a)}, ${show(m)})`, () => fs.lutimesSync(f, a, m));
  await promised(`fsp.utimes(f, ${show(a)}, ${show(m)})`, () => fsp.utimes(f, a, m));
  await promised(`fsp.lutimes(f, ${show(a)}, ${show(m)})`, () => fsp.lutimes(f, a, m));
  const fd = fs.openSync(f, "r+");
  sync(`futimesSync(fd, ${show(a)}, ${show(m)})`, () => fs.futimesSync(fd, a, m));
  fs.closeSync(fd);
}
console.log("lengths", fs.truncate.length, fs.truncateSync.length, fsp.truncate.length, fs.chmod.length, fs.chown.length, fs.lchown.length, fs.utimes.length);

fs.chmodSync(f, 0o666);
fs.rmSync(dir, { recursive: true, force: true });
