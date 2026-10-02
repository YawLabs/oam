// FileHandle.write / chmod / chown / truncate check their arguments with
// node's validators, as the descriptor calls of node:fs do.
//
// node v22.22.2 (lib/internal/fs/promises.js), after the closed-handle check:
// - write: (buffer[, offset[, length[, position]]]) or (buffer, options) with
//   fs.writeSync's checks, and (string[, position[, encoding]]) -- the second
//   argument of a string write is a POSITION; an empty view resolves 0 first;
// - chmod: parseFileMode; chown: uid / gid integers in [-1, 2**32-1];
//   truncate: an integer length, a negative one 0;
// - read / readv / write / writev resolve null-prototype objects.
// oam ran none of these: fh.write('XY', 3) took 3 as an offset into the
// string and wrote nothing, fh.write(5) wrote "5", fh.truncate('x') emptied
// the file and fh.chmod('zz') made it read-only.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import util from "node:util";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-261-"));
const file = path.join(dir, "f.txt");
const content = () => JSON.stringify(fs.readFileSync(file, "latin1"));
const mode = () => (fs.statSync(file).mode & 0o200 ? "writable" : "read-only");

function shape(e) {
  return `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`;
}
async function run(label, fn) {
  if (fs.existsSync(file)) fs.chmodSync(file, 0o666);
  fs.writeFileSync(file, "ABCDEF");
  const fh = await fsp.open(file, "r+");
  try {
    const r = await fn(fh);
    console.log(label, "->", util.inspect(r, { breakLength: Infinity }));
  } catch (e) {
    console.log(label, "rejects", shape(e));
  }
  await fh.close();
  console.log("   file", content(), mode());
}

await run("write('XY', 3)", (h) => h.write("XY", 3));
await run("write('6869', null, 'hex')", (h) => h.write("6869", null, "hex"));
await run("write('686', null, 'hex')", (h) => h.write("686", null, "hex"));
await run("write('é', 1, 'bogus')", (h) => h.write("é", 1, "bogus"));
await run("write('xy')", (h) => h.write("xy"));
await run("write('xy', -1)", (h) => h.write("xy", -1));
await run("write('xy', 1.5)", (h) => h.write("xy", 1.5));
await run("write('xy', '2')", (h) => h.write("xy", "2"));
await run("write('')", (h) => h.write(""));
for (const bad of [5, {}, null, undefined, 1n]) {
  await run(`write(${String(bad)})`, (h) => h.write(bad));
}
await run("write(buf, -1)", (h) => h.write(Buffer.from("a"), -1));
await run("write(buf, 1.5)", (h) => h.write(Buffer.from("a"), 1.5));
await run("write(buf, 0, 5)", (h) => h.write(Buffer.from("a"), 0, 5));
await run("write(buf, 2)", (h) => h.write(Buffer.from("a"), 2));
await run("write(buf, 1)", (h) => h.write(Buffer.from("a"), 1));
await run("write(buf, 0, 1, 3)", (h) => h.write(Buffer.from("q"), 0, 1, 3));
await run("write(buf, 0, 1, '3')", (h) => h.write(Buffer.from("q"), 0, 1, "3"));
await run("write(buf, 0, 1, 3n)", (h) => h.write(Buffer.from("q"), 0, 1, 3n));
await run("write(buf, {offset: 1, position: 2})", (h) => h.write(Buffer.from("qr"), { offset: 1, position: 2 }));
await run("write(buf, null)", (h) => h.write(Buffer.from("qr"), null));
await run("write(buf, 0, 'x')", (h) => h.write(Buffer.from("qr"), 0, "x"));
await run("write(empty, 'x')", (h) => h.write(Buffer.alloc(0), "x"));
await run("write(DataView, 1, 2, 0)", (h) => h.write(new DataView(new Uint8Array([119, 120, 121, 122]).buffer), 1, 2, 0));
await run("write(Uint16Array, 1, 2, 0)", (h) => h.write(new Uint16Array([0x4141, 0x4242]), 1, 2, 0));
for (const m of ["9", "zz", -1, 2 ** 32, 1.5, undefined, null, "444"]) {
  await run(`chmod(${util.inspect(m)})`, (h) => h.chmod(m));
}
for (const l of [null, "x", 1.5, NaN, 3n, -3, 2, undefined]) {
  await run(`truncate(${util.inspect(l)})`, (h) => h.truncate(l));
}
await run("truncate()", (h) => h.truncate());
for (const [u, g] of [[1.5, 0], [-2, 0], [0, "x"], [2 ** 32, 0], [0, -2], [undefined, 0], [-1, -1]]) {
  await run(`chown(${util.inspect(u)}, ${util.inspect(g)})`, (h) => h.chown(u, g));
}
await run("read", (h) => h.read(Buffer.alloc(2), 0, 2, 1));
await run("readv", (h) => h.readv([Buffer.alloc(2)], 4));
await run("writev", (h) => h.writev([Buffer.from("v")], 2));

// A closed handle is "file closed" before any argument is looked at.
const closed = await fsp.open(file, "r");
await closed.close();
for (const [label, fn] of [
  ["write(5)", () => closed.write(5)],
  ["chmod('zz')", () => closed.chmod("zz")],
  ["truncate('x')", () => closed.truncate("x")],
  ["chown(-2, 0)", () => closed.chown(-2, 0)],
]) {
  try {
    await fn();
    console.log("closed", label, "resolves");
  } catch (e) {
    console.log("closed", label, "rejects", shape(e), e.syscall);
  }
}
const fh = await fsp.open(file, "r+");
console.log("lengths", fh.write.length, fh.chmod.length, fh.chown.length, fh.truncate.length, fh.writeFile.length);
await fh.close();

fs.rmSync(dir, { recursive: true, force: true });
