// The other arguments of the fs descriptor calls, which node validates in
// JS BEFORE the descriptor reaches its binding.
//
// So a call with a bad descriptor and a bad other argument reports the
// other argument, and a bad argument with a good descriptor is refused rather
// than handed to the OS. oam validated none of these: fchmodSync(fd, 'zz')
// changed the file's mode to 0, ftruncateSync(fd, null) truncated to 0,
// readSync(fd, b, 0, 2, -2) read from the cursor, writeSync(fd, 5) and
// writeFileSync(fd, 5) wrote nothing and returned, and with -1 as the
// descriptor every one of them reported the descriptor instead.
//
// node v22.22.2 (lib/fs.js, lib/internal/fs/utils.js):
// - fchmod / fchmodSync: the mode through parseFileMode (an octal string or
//   a uint32), before the callback;
// - ftruncate / ftruncateSync: the length an integer (a negative one is 0);
// - fchown / fchownSync: uid and gid integers in [-1, 2**32-1];
// - read / readSync / FileHandle.read: the position through validatePosition
//   (an integer >= -1 or a bigint); -1 and null read at the cursor;
// - write / writeSync: the data a string or a view, the offset an integer,
//   the length an int32 within the data, an options object in the offset's
//   place; the position is NOT validated -- anything but a safe integer >= 0
//   writes at the cursor;
// - writeFile / appendFile and their Sync forms: the data a string or a view,
//   before the descriptor (and before the path).
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-257-"));
const file = path.join(dir, "f.txt");
const reset = () => fs.writeFileSync(file, "ABCDEFGHIJ");
reset();

function shape(e) {
  return `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`;
}
function show(v) {
  return typeof v === "bigint" ? `${v}n` : typeof v === "string" ? `'${v}'` : Array.isArray(v) ? "[1]" : v === null ? "null" : typeof v === "object" ? "{}" : String(v);
}
function sync(label, fn) {
  try {
    const r = fn();
    console.log(label, "->", r === undefined ? "undefined" : JSON.stringify(r));
  } catch (e) {
    console.log(label, "!!", shape(e));
  }
}
// A callback call: what is thrown at the call, or what the callback gets.
function settle(label, fn) {
  return new Promise((resolve) => {
    try {
      fn((err, n) => {
        console.log(label, "cb", err ? shape(err) : "ok " + (n === undefined ? "" : n));
        resolve();
      });
    } catch (e) {
      console.log(label, "!!", shape(e));
      resolve();
    }
  });
}
// A fresh descriptor per positional probe, so the cursor starts at 0.
function withFd(flags, fn) {
  reset();
  const fd = fs.openSync(file, flags);
  try {
    return fn(fd);
  } finally {
    fs.closeSync(fd);
  }
}
const content = () => JSON.stringify(fs.readFileSync(file, "latin1"));

// ---- fchmod: the mode first.
for (const m of ["zz", "0o644", -1, 1.5, 2 ** 32, undefined, null, true]) {
  sync(`fchmodSync(-1, ${show(m)})`, () => fs.fchmodSync(-1, m));
  await settle(`fchmod(-1, ${show(m)}, cb)`, (cb) => fs.fchmod(-1, m, cb));
}
sync("fchmod(-1, 'zz', 5)", () => fs.fchmod(-1, "zz", 5));
sync("fchmod(-1, 0o644, 5)", () => fs.fchmod(-1, 0o644, 5));
sync("fchmodSync(-1, '644')", () => fs.fchmodSync(-1, "644"));
withFd("r+", (fd) => {
  sync("fchmodSync(fd, 'zz')", () => fs.fchmodSync(fd, "zz"));
  sync("fchmodSync(fd, '644')", () => fs.fchmodSync(fd, "644"));
  sync("fchmodSync(fd, 0o666)", () => fs.fchmodSync(fd, 0o666));
});

// ---- ftruncate: the length, an integer; a negative one is 0.
for (const l of ["x", 1.5, NaN, 2 ** 53, null, 3n]) {
  sync(`ftruncateSync(-1, ${show(l)})`, () => fs.ftruncateSync(-1, l));
  await settle(`ftruncate(-1, ${show(l)}, cb)`, (cb) => fs.ftruncate(-1, l, cb));
}
sync("ftruncate(-1, 'x', 5)", () => fs.ftruncate(-1, "x", 5));
sync("ftruncate(-1, 'x')", () => fs.ftruncate(-1, "x"));
sync("ftruncateSync(-1)", () => fs.ftruncateSync(-1));
withFd("r+", (fd) => {
  sync("ftruncateSync(fd, null)", () => fs.ftruncateSync(fd, null));
  sync("ftruncateSync(fd, -5)", () => [fs.ftruncateSync(fd, -5), content()]);
});
withFd("r+", (fd) => sync("ftruncateSync(fd, 4)", () => [fs.ftruncateSync(fd, 4), content()]));
withFd("r+", (fd) => sync("ftruncateSync(fd)", () => [fs.ftruncateSync(fd), content()]));
{
  reset();
  const fd = fs.openSync(file, "r+");
  await settle("ftruncate(fd, -3, cb)", (cb) => fs.ftruncate(fd, -3, cb));
  console.log("after", content());
  fs.closeSync(fd);
}
console.log("arity", fs.ftruncateSync.length, fs.ftruncate.length, fs.fchmodSync.length, fs.fchownSync.length);

// ---- fchown: uid then gid, integers in [-1, 2**32-1].
for (const [u, g] of [[-2, 0], [0, -2], [2 ** 32, 0], [1.5, 0], ["x", 0], [undefined, 0], [0, null]]) {
  sync(`fchownSync(-1, ${show(u)}, ${show(g)})`, () => fs.fchownSync(-1, u, g));
  await settle(`fchown(-1, ${show(u)}, ${show(g)}, cb)`, (cb) => fs.fchown(-1, u, g, cb));
}
sync("fchownSync(-1, -1, -1)", () => fs.fchownSync(-1, -1, -1));
sync("fchownSync(-1, 2 ** 32 - 1, 0)", () => fs.fchownSync(-1, 2 ** 32 - 1, 0));
sync("fchown(-1, 'x', 0, 5)", () => fs.fchown(-1, "x", 0, 5));

// ---- read position: validatePosition, before the descriptor.
const B = Buffer.alloc(4);
for (const p of ["zz", -2, 1.5, NaN, Infinity, 2 ** 53, true, {}, -2n, 2n ** 63n]) {
  sync(`readSync(-1, B, 0, 2, ${show(p)})`, () => fs.readSync(-1, B, 0, 2, p));
  sync(`readSync(-1, B, {position: ${show(p)}})`, () => fs.readSync(-1, B, { length: 2, position: p }));
  sync(`read(-1 -> 9, B, 0, 2, ${show(p)}, cb)`, () => fs.read(9, B, 0, 2, p, () => {}));
}
sync("readSync(-1, B, 0, 0, 'zz')", () => fs.readSync(-1, B, 0, 0, "zz"));
sync("readSync(-1, B, 0, 2, 2n ** 63n - 3n)", () => fs.readSync(-1, B, 0, 2, 2n ** 63n - 3n));
for (const p of [-1, null, undefined, 6, 6n, 2 ** 53 - 1]) {
  withFd("r", (fd) => {
    sync(`readSync(fd, B, 0, 2, ${show(p)}) then cursor`, () => {
      const n = fs.readSync(fd, B, 0, 2, p);
      const got = B.toString("latin1", 0, n);
      const c = Buffer.alloc(2);
      fs.readSync(fd, c, 0, 2, null);
      return [n, got, c.toString()];
    });
  });
}
for (const p of [-1, 6n, "zz"]) {
  reset();
  const fd = fs.openSync(file, "r");
  await settle(`read(fd, B, 0, 2, ${show(p)}, cb)`, (cb) => fs.read(fd, B, 0, 2, p, cb));
  fs.closeSync(fd);
}
for (const p of ["zz", -2, 1.5, 3n, -1]) {
  reset();
  const fh = await fsp.open(file, "r");
  try {
    const r = await fh.read(Buffer.alloc(2), 0, 2, p);
    console.log(`fh.read(b, 0, 2, ${show(p)}) ->`, r.bytesRead, r.buffer.toString());
  } catch (e) {
    console.log(`fh.read(b, 0, 2, ${show(p)}) !!`, shape(e));
  }
  await fh.close();
}

// ---- write / writeSync: data, offset, length; never the position.
for (const d of [5, {}, null, undefined, [1], 1n]) {
  sync(`writeSync(-1, ${show(d)})`, () => fs.writeSync(-1, d));
  sync(`write(-1 -> 9, ${show(d)}, cb)`, () => fs.write(9, d, () => {}));
}
const W = Buffer.from("wxyz");
for (const o of [-1, 1.5, "x", 5, NaN, 2 ** 53]) {
  sync(`writeSync(-1, W, ${show(o)})`, () => fs.writeSync(-1, W, o));
  sync(`write(9, W, ${show(o)}, 0, 0, cb)`, () => fs.write(9, W, o, 0, 0, () => {}));
}
for (const l of [-1, 1.5, 5, 2 ** 31, NaN]) {
  sync(`writeSync(-1, W, 0, ${show(l)})`, () => fs.writeSync(-1, W, 0, l));
  sync(`write(9, W, 0, ${show(l)}, 0, cb)`, () => fs.write(9, W, 0, l, 0, () => {}));
}
sync("writeSync(-1, W, {offset: -1})", () => fs.writeSync(-1, W, { offset: -1 }));
sync("writeSync(-1, W, {length: 9})", () => fs.writeSync(-1, W, { length: 9 }));
sync("write(9, W, {offset: 'x'}, cb)", () => fs.write(9, W, { offset: "x" }, () => {}));
sync("writeSync(-1, W, 4)", () => fs.writeSync(-1, W, 4));
sync("writeSync(-1, W, 0, 'x')", () => fs.writeSync(-1, W, 0, "x"));
sync("writeSync(-1, 'abc', 0, 'hex')", () => fs.writeSync(-1, "abc", 0, "hex"));
sync("write(9, 'abc', 0, 'hex', cb)", () => fs.write(9, "abc", 0, "hex", () => {}));
sync("write(9, 5) no callback", () => fs.write(9, 5));
sync("write(9, W) no callback", () => fs.write(9, W));
for (const p of [1.5, -2, "x", 2n, null, 3]) {
  withFd("r+", (fd) => sync(`writeSync(fd, W, 0, 1, ${show(p)})`, () => [fs.writeSync(fd, W, 0, 1, p), content()]));
  withFd("r+", (fd) => sync(`writeSync(fd, 'q', ${show(p)})`, () => [fs.writeSync(fd, "q", p), content()]));
}
withFd("r+", (fd) => sync("writeSync(fd, W, {offset: 1, length: 2, position: 5})", () => [fs.writeSync(fd, W, { offset: 1, length: 2, position: 5 }), content()]));
withFd("r+", (fd) => sync("writeSync(fd, W, null)", () => [fs.writeSync(fd, W, null), content()]));
withFd("r+", (fd) => sync("writeSync(fd, DataView, 1, 2, 0)", () => [fs.writeSync(fd, new DataView(W.buffer, W.byteOffset, 4), 1, 2, 0), content()]));
withFd("r+", (fd) => sync("writeSync(fd, Uint16Array, 1, 2, 0)", () => [fs.writeSync(fd, new Uint16Array([0x4141, 0x4242]), 1, 2, 0), content()]));
withFd("r+", (fd) => sync("writeSync(fd, '4142', 0, 'hex')", () => [fs.writeSync(fd, "4142", 0, "hex"), content()]));
// An encoding the binding does not know is UTF-8, not an error -- so with a
// bad descriptor the descriptor is what is reported.
for (const enc of ["bogus", "buffer", "UTF16LE", 5]) {
  withFd("r+", (fd) => sync(`writeSync(fd, 'é', 0, ${show(enc)})`, () => [fs.writeSync(fd, "é", 0, enc), content()]));
}
sync("writeSync(-1, 'str', 0, 'bogus')", () => fs.writeSync(-1, "str", 0, "bogus"));
sync("write(-1, 'str', 0, 'bogus', cb)", () => fs.write(-1, "str", 0, "bogus", () => {}));
{
  reset();
  const fd = fs.openSync(file, "r+");
  await settle("write(fd, 'é', 0, 'bogus', cb)", (cb) => fs.write(fd, "é", 0, "bogus", cb));
  console.log("  file", content());
  fs.closeSync(fd);
}
for (const [label, args] of [
  ["write(fd, W, 1.5 position)", (fd, cb) => fs.write(fd, W, 0, 1, 1.5, cb)],
  ["write(fd, 'x', 1.5, cb)", (fd, cb) => fs.write(fd, "x", 1.5, cb)],
  ["write(fd, W, cb)", (fd, cb) => fs.write(fd, W, cb)],
  ["write(fd, W, 1, cb)", (fd, cb) => fs.write(fd, W, 1, cb)],
  ["write(fd, W, 1, 2, cb)", (fd, cb) => fs.write(fd, W, 1, 2, cb)],
  ["write(fd, W, {offset: 2}, cb)", (fd, cb) => fs.write(fd, W, { offset: 2 }, cb)],
  ["write(fd, 'xy', 3, 'latin1', cb)", (fd, cb) => fs.write(fd, "xy", 3, "latin1", cb)],
  ["write(fd, 'xy', 3, cb)", (fd, cb) => fs.write(fd, "xy", 3, cb)],
]) {
  reset();
  const fd = fs.openSync(file, "r+");
  await settle(label, (cb) => args(fd, cb));
  console.log("  file", content());
  fs.closeSync(fd);
}

// ---- writeFile / appendFile: the data before the descriptor or path.
for (const d of [5, {}, null, [1]]) {
  sync(`writeFile(-1, ${show(d)}, cb)`, () => fs.writeFile(-1, d, () => {}));
  sync(`appendFile(-1, ${show(d)}, cb)`, () => fs.appendFile(-1, d, () => {}));
  sync(`writeFile(path, ${show(d)}, cb)`, () => fs.writeFile(file, d, () => {}));
  sync(`writeFileSync(-1, ${show(d)})`, () => fs.writeFileSync(-1, d));
  sync(`appendFileSync(-1, ${show(d)})`, () => fs.appendFileSync(-1, d));
  sync(`writeFileSync(path, ${show(d)})`, () => fs.writeFileSync(file, d));
}
sync("writeFile(-1, 5) no callback", () => fs.writeFile(-1, 5));
withFd("r+", (fd) => sync("writeFileSync(fd, DataView)", () => [fs.writeFileSync(fd, new DataView(W.buffer, W.byteOffset, 2)), content()]));
console.log("final", content());

fs.rmSync(dir, { recursive: true, force: true });
