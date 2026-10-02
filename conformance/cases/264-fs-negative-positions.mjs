// A negative write / readv position other than -1, as node's binding hands it
// to libuv: the cursor on unix (uv__fs_write / uv__fs_read take any off < 0
// as write(2) / read(2)). On Windows fs__write / fs__read give every offset
// but -1 to the OS as an OVERLAPPED offset around a saved and restored file
// pointer: -2 is the OS's "current position", so the op lands at the cursor
// and the cursor stays put; an append handle appends; anything else is
// EINVAL with the file untouched. EBADF still comes first for a closed
// descriptor. -1, -0, a fraction and a bigint are the cursor everywhere. oam
// turned every negative into "the cursor", so on Windows it wrote where node
// fails.
//
// The expected output differs by platform; node and oam agree on each.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
const fsp = fs.promises;
const d = fs.mkdtempSync(path.join(os.tmpdir(), "oam-264-"));
const p = path.join(d, "f");
const c = () => fs.readFileSync(p, "latin1");
const sh = (e) => e ? `${e.code} ${e.syscall} ${JSON.stringify(e.message)} ${Object.keys(e)}` : "null";

for (const [label, run] of [
    ["writeSync buf -5", (fd) => fs.writeSync(fd, Buffer.from("x"), 0, 1, -5)],
    ["writeSync str -5", (fd) => fs.writeSync(fd, "w", -5)],
    ["writeSync buf -1", (fd) => fs.writeSync(fd, Buffer.from("x"), 0, 1, -1)],
    ["writeSync buf -0", (fd) => fs.writeSync(fd, Buffer.from("x"), 0, 1, -0)],
    ["writeSync buf -1.5", (fd) => fs.writeSync(fd, Buffer.from("x"), 0, 1, -1.5)],
    ["writeSync buf -2n", (fd) => fs.writeSync(fd, Buffer.from("x"), 0, 1, -2n)],
    ["writeSync buf 3", (fd) => fs.writeSync(fd, Buffer.from("x"), 0, 1, 3)],
    ["writeSync -2 twice, then the cursor", (fd) => [fs.writeSync(fd, "q", -2), fs.writeSync(fd, "r", -2), fs.writeSync(fd, "s")]],
    ["read 3, writeSync -2, then the cursor", (fd) => { fs.readSync(fd, Buffer.alloc(3)); return [fs.writeSync(fd, "q", -2), fs.writeSync(fd, "s")]; }],
    ["writeSync 3 bytes -2", (fd) => [fs.writeSync(fd, Buffer.alloc(3, 0x7a), 0, 3, -2), fs.writeSync(fd, "s")]],
    ["readvSync 1 view -2, then the cursor", (fd) => { const b = Buffer.alloc(2); const n = fs.readvSync(fd, [b], -2); const b2 = Buffer.alloc(1); fs.readSync(fd, b2); return [n, b.toString(), b2.toString()]; }],
    ["writevSync -3", (fd) => fs.writevSync(fd, [Buffer.from("v")], -3)],
    ["readvSync -3", (fd) => { const b = Buffer.alloc(2); const n = fs.readvSync(fd, [b], -3); return [n, b.toString()]; }],
    ["writeSync buf -2**53+1", (fd) => fs.writeSync(fd, Buffer.from("x"), 0, 1, -(2 ** 53) + 1)],
    ["writeSync empty -5", (fd) => fs.writeSync(fd, Buffer.alloc(0), 0, 0, -5)],
  ]) {
    fs.writeFileSync(p, "ABCDEFGHIJ"); const fd = fs.openSync(p, "r+");
    try { console.log(label, JSON.stringify(run(fd)), c()); } catch (e) { console.log(label, "!!", sh(e), c()); }
    fs.closeSync(fd);
  }
  for (const [label, run] of [
    ["write buf -7", (fd, cb) => fs.write(fd, Buffer.from("q"), 0, 1, -7, cb)],
    ["write str -7", (fd, cb) => fs.write(fd, "q", -7, cb)],
    ["writev -3", (fd, cb) => fs.writev(fd, [Buffer.from("v")], -3, cb)],
    ["readv -3", (fd, cb) => fs.readv(fd, [Buffer.alloc(2)], -3, cb)],
  ]) {
    fs.writeFileSync(p, "ABCDEFGHIJ"); const fd = fs.openSync(p, "r+");
    await new Promise((r) => run(fd, (e, n) => { console.log(label, sh(e), n, c()); r(); }));
    fs.closeSync(fd);
  }
  for (const [label, run] of [
    ["fh.write buf -3", (h) => h.write(Buffer.from("h"), 0, 1, -3)],
    ["fh.write str -3", (h) => h.write("h", -3)],
    ["fh.writev -3", (h) => h.writev([Buffer.from("h")], -3)],
    ["fh.readv -3", (h) => h.readv([Buffer.alloc(2)], -3)],
  ]) {
    fs.writeFileSync(p, "ABCDEFGHIJ"); const h = await fsp.open(p, "r+");
    try { const r = await run(h); console.log(label, r.bytesWritten ?? r.bytesRead, c()); } catch (e) { console.log(label, "rejects", sh(e), c()); }
    await h.close();
  }
  for (const pos of [-2, -5]) {
    fs.writeFileSync(p, "ABCDEFGHIJ");
    const fd = fs.openSync(p, "a+");
    try { console.log("append handle", pos, fs.writeSync(fd, "q", pos), c()); } catch (e) { console.log("append handle", pos, "!!", sh(e), c()); }
    fs.closeSync(fd);
  }
  // closed fd + negative position: EBADF wins
  const fd = fs.openSync(p, "r+"); fs.closeSync(fd);
  try { fs.writeSync(fd, Buffer.from("x"), 0, 1, -5); } catch (e) { console.log("closed", sh(e)); }
  fs.rmSync(d, { recursive: true });

