// fd-operation ERROR SHAPES, and the positional (pread/pwrite) overloads.
//
// Case 78 pins the readv/writev shapes; this pins the edges around them that
// 78 does not reach, each one measured against node rather than reasoned about:
//
//   - a genuinely EMPTY path keeps its quotes ("open ''"). Absence of a path
//     (an fd call) is a different thing and prints no segment at all -- the two
//     cannot be told apart by testing `path === ""`.
//   - an fd error has NO `path` property. `Object.keys(err)` is observable and
//     node's is ["errno","code","syscall"] -- except fs.writeSync, the one call
//     that builds its error in JS from a libuv ctx and yields
//     ["errno","syscall","code"]. fs.write and fs.writevSync use the common
//     order, so it is a property of that call site, not of the `write` syscall.
//   - a FAILED read/write does not close the descriptor. It stays usable.
//   - only an EMPTY LIST skips the fd. An array of zero-length views still
//     issues the syscall, so it still reports EBADF on a closed or wrong-mode
//     descriptor.
//   - readv/writev throw ERR_INVALID_ARG_TYPE SYNCHRONOUSLY (node's
//     validateBufferArray runs at the call site) while the empty-list EINVAL is
//     delivered to the callback. The two failures land differently.
//   - `position` is honoured by EVERY overload: fs.read's options form and
//     FileHandle.read/write, not just the positional-argument form.
//   - a read is bounded by its destination buffer (ERR_OUT_OF_RANGE), and is
//     NOT capped below that -- a >8 MiB read returns all of it.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const file = path.join(os.tmpdir(), `oam-conf-fderr-${process.pid}.txt`);
const show = (label, fn) => {
  try {
    console.log(label, "=>", JSON.stringify(fn()));
  } catch (e) {
    console.log(label, "=> THROW", e.code, JSON.stringify(e.message), JSON.stringify(Object.keys(e)));
  }
};

// --- an empty path is a path: node prints the quotes and sets err.path = ""
show("openSync('')           ", () => fs.openSync(""));
show("statSync('')           ", () => fs.statSync(""));
try {
  fs.openSync("");
} catch (e) {
  console.log("  err.path =", JSON.stringify(e.path));
}

// --- an fd error has no path at all, and its own-key ORDER is node's
fs.writeFileSync(file, "ABCDEFGHIJKLM");
const rfd = fs.openSync(file, "r");
show("writeSync(r-fd)        ", () => fs.writeSync(rfd, Buffer.from("x")));
show("writevSync(r-fd)       ", () => fs.writevSync(rfd, [Buffer.from("x")]));
show("fstatSync(bad fd)      ", () => fs.fstatSync(9999));

// --- a failed operation must NOT retire the descriptor
await new Promise((resolve) =>
  fs.write(rfd, Buffer.from("x"), (err) => {
    console.log("async write(r-fd):", err && err.code, JSON.stringify(Object.keys(err ?? {})));
    resolve();
  }),
);
show("readSync after failure ", () => fs.readSync(rfd, Buffer.alloc(3), 0, 3, 0));
show("closeSync after failure", () => {
  fs.closeSync(rfd);
  return "ok";
});

// --- only the EMPTY LIST skips the fd; zero-length views still syscall
//
// `ro` is opened BEFORE `closed` is retired, deliberately: node hands out the
// lowest free descriptor, so opening anything after the close would silently
// recycle that number and the "closed fd" assertions below would run against a
// live one. (oam allocates handles from a monotonic counter and never reuses,
// so the ordering only matters for node -- which is exactly why the case has
// to be written to node's rule.)
const ro = fs.openSync(file, "r");
const closed = fs.openSync(file, "r");
fs.closeSync(closed);
show("readvSync(closed,[0len])", () => fs.readvSync(closed, [Buffer.alloc(0)]));
show("writevSync(closed,[0len])", () => fs.writevSync(closed, [Buffer.alloc(0)]));
show("writevSync(closed,[])   ", () => fs.writevSync(closed, []));
show("writevSync(r-fd,[0len]) ", () => fs.writevSync(ro, [Buffer.alloc(0)]));

// The async twin of the same rule, including that the rejection carries node's
// key order rather than the sync path's.
await new Promise((resolve) =>
  fs.readv(closed, [Buffer.alloc(0)], (err, n) => {
    console.log("readv(closed,[0len]):", err ? err.code : n, JSON.stringify(Object.keys(err ?? {})));
    resolve();
  }),
);
await new Promise((resolve) =>
  fs.readv(closed, [], (err, n, bufs) => {
    console.log("readv(closed,[]):", err && err.code, "n =", n, "| same array:", Array.isArray(bufs));
    resolve();
  }),
);

// --- arg-type is SYNCHRONOUS even in the callback form
show("readv(['x'],cb)        ", () => {
  fs.readv(ro, ["x"], () => {});
  return "no throw";
});
show("writev(['x'],cb)       ", () => {
  fs.writev(ro, ["x"], () => {});
  return "no throw";
});

// --- position, in every overload that accepts one
const viaOptions = Buffer.alloc(3);
await new Promise((resolve) =>
  fs.read(ro, { buffer: viaOptions, offset: 0, length: 3, position: 10 }, () => {
    console.log("fs.read(options) pos=10 =>", JSON.stringify(viaOptions.toString()));
    resolve();
  }),
);
fs.closeSync(ro);

const fh = await fsp.open(file, "r+");
const viaHandle = Buffer.alloc(3);
const readResult = await fh.read(viaHandle, 0, 3, 10);
console.log("FileHandle.read pos=10 =>", JSON.stringify(viaHandle.toString()), "bytesRead", readResult.bytesRead);
// A positional write is a pwrite: it lands at 1 and leaves the cursor at 0.
await fh.write(Buffer.from("zz"), 0, 2, 1);
const afterWrite = Buffer.alloc(5);
await fh.read(afterWrite, 0, 5, 0);
console.log("FileHandle.write pos=1 =>", JSON.stringify(afterWrite.toString()));
await fh.close();

// --- bounded by the destination, but not capped below it
const bounded = fs.openSync(file, "r");
show("readSync len > buffer  ", () => fs.readSync(bounded, Buffer.alloc(4), 0, 100, null));
fs.closeSync(bounded);

const big = path.join(os.tmpdir(), `oam-conf-fderr-big-${process.pid}.bin`);
const size = 9 * 1024 * 1024; // over the 8 MiB ceiling the async path used to impose
fs.writeFileSync(big, Buffer.alloc(size, 0x41));
const bfd = fs.openSync(big, "r");
console.log("readvSync 9MiB =>", fs.readvSync(bfd, [Buffer.alloc(size / 2), Buffer.alloc(size / 2)], 0));
await new Promise((resolve) =>
  fs.readv(bfd, [Buffer.alloc(size / 2), Buffer.alloc(size / 2)], 0, (err, n) => {
    console.log("readv    9MiB =>", err ? err.code : n);
    resolve();
  }),
);
fs.closeSync(bfd);
fs.unlinkSync(big);
fs.unlinkSync(file);
