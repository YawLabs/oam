// A file descriptor is a file descriptor, whichever half of the fs API opened
// it. Node has ONE descriptor space; oam used to have two.
//
// oam kept a tokio-backed registry for the async fs ops and a std-backed one
// for the sync ops, and an fd allocated in one was invisible to the other:
//
//   const fd = await promisified(fs.open)(p, 'r');
//   fs.readSync(fd, ...)   // node: reads.  oam: EBADF
//
// Both families always drew from the same id counter, so the numbers never
// collided -- it was purely a failed lookup, which is why it failed loudly
// instead of reading the wrong file.
//
// The reverse direction matters just as much: an fd from openSync handed to the
// callback API. Both are exercised here, along with the property that makes
// this more than cosmetic -- the two halves must share one CURSOR, not just
// find each other.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const file = path.join(os.tmpdir(), `oam-conf-fdspace-${process.pid}.txt`);
fs.writeFileSync(file, "ABCDEFGHIJKLMNOPQRSTUVWXYZ");

const open = (p, flags) =>
  new Promise((resolve, reject) => fs.open(p, flags, (e, fd) => (e ? reject(e) : resolve(fd))));
const close = (fd) =>
  new Promise((resolve, reject) => fs.close(fd, (e) => (e ? reject(e) : resolve())));
const read = (fd, buf, off, len, pos) =>
  new Promise((resolve, reject) =>
    fs.read(fd, buf, off, len, pos, (e, n) => (e ? reject(e) : resolve(n))),
  );

// --- async-opened fd, used by the sync family
{
  const fd = await open(file, "r");
  const b = Buffer.alloc(4);
  fs.readSync(fd, b, 0, 4, 0);
  console.log("readSync on async fd:", b.toString());
  console.log("fstatSync on async fd size:", fs.fstatSync(fd).size);
  fs.closeSync(fd);
  console.log("closeSync on async fd: ok");
}

// --- sync-opened fd, used by the async family
{
  const fd = fs.openSync(file, "r");
  const b = Buffer.alloc(4);
  const n = await read(fd, b, 0, 4, 0);
  console.log("read on sync fd:", n, b.toString());
  await close(fd);
  console.log("close on sync fd: ok");
}

// --- one descriptor means one CURSOR, shared across both halves
{
  const fd = await open(file, "r");
  const a = Buffer.alloc(4);
  fs.readSync(fd, a, 0, 4, null); // sync read advances the cursor
  const b = Buffer.alloc(4);
  await read(fd, b, 0, 4, null); // async read continues from there
  const c = Buffer.alloc(4);
  fs.readSync(fd, c, 0, 4, null); // and back again
  console.log("interleaved sync/async reads:", JSON.stringify([a.toString(), b.toString(), c.toString()]));
  fs.closeSync(fd);
}

// --- a closed fd is closed for BOTH halves
{
  const fd = fs.openSync(file, "r");
  await close(fd); // closed through the async API
  let syncCode = null;
  try {
    fs.readSync(fd, Buffer.alloc(4), 0, 4, 0);
  } catch (e) {
    syncCode = e.code;
  }
  console.log("sync read after async close:", syncCode);
}

fs.unlinkSync(file);
