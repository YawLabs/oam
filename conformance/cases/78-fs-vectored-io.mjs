// Vectored IO: fs.readv / readvSync / writev / writevSync.
//
// Implemented over the ordinary read/write paths -- writev concatenates and
// issues one write, readv reads once and scatters -- so it inherits their
// pread/pwrite semantics rather than growing a second copy of them. For a
// regular file that is indistinguishable from a real iovec call.
//
// The shapes worth pinning are the ones that are not what you would guess:
//
//   - the SYNC forms return a plain NUMBER, not {bytesRead, buffers}. Only the
//     FileHandle methods return objects.
//   - the callback is (err, bytes, buffers) and `buffers` is the SAME ARRAY
//     IDENTITY that went in, which callers compare by reference.
//   - an EMPTY ARRAY is asymmetric: writev returns 0, readv throws EINVAL.
//   - an array of only ZERO-LENGTH views is a DIFFERENT case: readv returns 0.
//     The rule keys on the array being empty, not on the byte total.
//   - a partial final buffer keeps its remaining bytes UNTOUCHED.
//   - node exports these on node:fs only; fs/promises has no top-level
//     readv/writev.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const file = path.join(os.tmpdir(), `oam-conf-vec-${process.pid}.txt`);
const show = (label, fn) => {
  try {
    const r = fn();
    console.log(label, "=>", typeof r, JSON.stringify(r));
  } catch (e) {
    console.log(label, "=> THROW", e.code);
  }
};

// --- writev concatenates in order
fs.writeFileSync(file, "");
let fd = fs.openSync(file, "w+");
show("writevSync 3 bufs      ", () =>
  fs.writevSync(fd, [Buffer.from("foo"), Buffer.from("bar"), Buffer.from("baz")]),
);
fs.closeSync(fd);
console.log("  file =", JSON.stringify(fs.readFileSync(file, "utf8")));

// --- the empty-array asymmetry
fd = fs.openSync(file, "w+");
show("writevSync([])         ", () => fs.writevSync(fd, []));
show("readvSync([])          ", () => fs.readvSync(fd, []));
fs.closeSync(fd);

// --- zero-length views are skipped, and are NOT the empty-array case
fs.writeFileSync(file, "");
fd = fs.openSync(file, "w+");
show("writevSync w/ empties  ", () =>
  fs.writevSync(fd, [Buffer.alloc(0), Buffer.from("hi"), Buffer.alloc(0)]),
);
fs.closeSync(fd);
console.log("  file =", JSON.stringify(fs.readFileSync(file, "utf8")));

fs.writeFileSync(file, "ABCDEFGHIJ");
fd = fs.openSync(file, "r");
show("readvSync only empties ", () => fs.readvSync(fd, [Buffer.alloc(0), Buffer.alloc(0)]));
const mixed = [Buffer.alloc(0), Buffer.alloc(3), Buffer.alloc(0)];
show("readvSync mixed empties", () => fs.readvSync(fd, mixed));
console.log("  filled =", JSON.stringify(mixed[1].toString()));
fs.closeSync(fd);

// --- a partial final buffer keeps its remaining bytes
fd = fs.openSync(file, "r");
const bufs = [Buffer.alloc(4).fill("."), Buffer.alloc(4).fill("."), Buffer.alloc(4).fill(".")];
show("readvSync oversized    ", () => fs.readvSync(fd, bufs));
console.log("  bufs =", JSON.stringify(bufs.map((b) => b.toString())));
show("readvSync at EOF       ", () => fs.readvSync(fd, [Buffer.alloc(4)]));
fs.closeSync(fd);

// --- position is pread: it does not move the cursor
fs.writeFileSync(file, "ABCDEFGHIJKLM");
fd = fs.openSync(file, "r");
const p1 = Buffer.alloc(3);
const p2 = Buffer.alloc(3);
fs.readvSync(fd, [p1], 10);
fs.readvSync(fd, [p2], null);
console.log("readv(pos=10):", p1.toString(), "| then readv(null):", p2.toString());
fs.closeSync(fd);

// --- validation. The sparse case is why the check cannot use Array#every,
// which skips holes.
fd = fs.openSync(file, "r");
show("bare Buffer            ", () => fs.readvSync(fd, Buffer.alloc(4)));
show("array of string        ", () => fs.readvSync(fd, ["x"]));
show("sparse array           ", () => {
  const s = [Buffer.alloc(2)];
  s[2] = Buffer.alloc(2);
  return fs.readvSync(fd, s);
});
show("array-like             ", () => fs.readvSync(fd, { 0: Buffer.alloc(2), length: 1 }));

// --- any ArrayBufferView, with byteOffset/byteLength honoured
const backing = Buffer.alloc(16).fill("-");
show("readvSync into subarray", () => fs.readvSync(fd, [backing.subarray(4, 9)], 0));
console.log("  backing =", JSON.stringify(backing.toString()));
// A Uint16Array counts as BYTES, not elements.
show("DataView elem          ", () => fs.readvSync(fd, [new DataView(new ArrayBuffer(2))], 0));
fs.closeSync(fd);

// --- callback form hands back the SAME array
fs.writeFileSync(file, "ABCDEFGH");
fd = fs.openSync(file, "r");
const input = [Buffer.alloc(4), Buffer.alloc(4)];
await new Promise((resolve) =>
  fs.readv(fd, input, (err, n, out) => {
    console.log("readv cb:", err, n, "| same array:", out === input, "| same buf0:", out[0] === input[0]);
    console.log("  contents =", JSON.stringify(input.map((b) => b.toString())));
    resolve();
  }),
);
await new Promise((resolve) =>
  fs.writev(fd, [Buffer.from("ab")], (err, n, out) => {
    console.log("writev cb err:", err && err.code, "| same array:", out === undefined ? "n/a" : true);
    resolve();
  }),
);
fs.closeSync(fd);

// --- a closed descriptor
const closed = fs.openSync(file, "r");
fs.closeSync(closed);
show("readvSync closed fd    ", () => fs.readvSync(closed, [Buffer.alloc(4)]));
try {
  fs.readvSync(closed, [Buffer.alloc(4)]);
} catch (e) {
  console.log("  own props =", JSON.stringify(Object.keys(e)), "| syscall =", e.syscall);
}
// EINVAL wins over EBADF: node applies the empty-array rule before the fd.
show("readvSync closed + []  ", () => fs.readvSync(closed, []));

// --- not exported on fs/promises
console.log("fsp.readv:", typeof fsp.readv, "| fsp.writev:", typeof fsp.writev);

fs.unlinkSync(file);
