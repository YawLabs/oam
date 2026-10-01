// Several asynchronous operations in flight on ONE descriptor at once.
//
// node runs each fs request on its thread pool against the same descriptor,
// so a second fs.read fired before the first has called back is served like
// the first. oam's async read and write took the file out of its descriptor
// table for the length of the IO, and every request that arrived meanwhile
// found the descriptor missing: all but the first of six fs.read calls
// called back EBADF, and so did a readv or writev racing a read.
//
// The results are printed in submission order, after every callback has run,
// so the output does not depend on which request finished first.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oam-256-"));
const file = path.join(dir, "data.txt");
const ALPHA = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
fs.writeFileSync(file, ALPHA);

function settle(start) {
  return new Promise((resolve) => {
    start((...args) => resolve(args));
  });
}
const code = (err) => (err ? err.code : null);

async function callbackForms() {
  const fd = fs.openSync(file, "r+");
  // Six positional reads, none awaited before the next is issued.
  const reads = [];
  for (let i = 0; i < 6; i++) {
    reads.push(settle((cb) => fs.read(fd, Buffer.alloc(2), 0, 2, i * 2, cb)));
  }
  for (const [i, [err, n, buf]] of (await Promise.all(reads)).entries()) {
    console.log("read", i, code(err), n, err ? "" : buf.toString());
  }
  // One positional read leaves the cursor alone. (Only one: on Windows,
  // libuv -- and oam, which does the same -- saves the cursor, reads at the
  // position and puts the cursor back, so where several CONCURRENT
  // positional reads leave it depends on their interleaving, in node too --
  // hence a descriptor of its own here.)
  const fresh = fs.openSync(file, "r");
  await settle((cb) => fs.read(fresh, Buffer.alloc(2), 0, 2, 12, cb));
  const head = Buffer.alloc(3);
  console.log("cursor read after one", fs.readSync(fresh, head, 0, 3, null), head.toString());
  fs.closeSync(fresh);

  // Every kind of request at once on the same descriptor.
  const mixed = [
    settle((cb) => fs.write(fd, "0", 10, cb)),
    settle((cb) => fs.write(fd, Buffer.from("1"), 0, 1, 11, cb)),
    settle((cb) => fs.writev(fd, [Buffer.from("2"), Buffer.from("3")], 12, cb)),
    settle((cb) => fs.read(fd, Buffer.alloc(2), 0, 2, 0, cb)),
    settle((cb) => fs.readv(fd, [Buffer.alloc(1), Buffer.alloc(1)], 2, cb)),
    settle((cb) => fs.fstat(fd, cb)),
    settle((cb) => fs.fsync(fd, cb)),
    settle((cb) => fs.fdatasync(fd, cb)),
    settle((cb) => fs.futimes(fd, 1000, 2000, cb)),
  ];
  const labels = ["write string", "write buffer", "writev", "read", "readv", "fstat", "fsync", "fdatasync", "futimes"];
  for (const [i, [err, a, b]] of (await Promise.all(mixed)).entries()) {
    let detail = "";
    if (!err) {
      if (labels[i] === "read") detail = b.toString();
      else if (labels[i] === "readv") detail = b.map(String).join("");
      else if (labels[i] === "fstat") detail = `size ${a.size}`;
      else if (typeof a === "number") detail = `n ${a}`;
    }
    console.log(labels[i], code(err), detail);
  }
  console.log("file", fs.readFileSync(file, "utf8"));

  // Cursor (non-positional) reads in flight together consume the file
  // between them: every byte is read exactly once.
  const cursorFd = fs.openSync(file, "r");
  const seq = [];
  for (let i = 0; i < 4; i++) {
    seq.push(settle((cb) => fs.read(cursorFd, Buffer.alloc(4), 0, 4, null, cb)));
  }
  const got = (await Promise.all(seq)).map(([err, n, buf]) => (err ? err.code : buf.toString("latin1", 0, n)));
  console.log("cursor reads", got.map((s) => s.length).join(","), got.join("").split("").sort().join(""));
  fs.closeSync(cursorFd);
  fs.closeSync(fd);
  // A closed descriptor is EBADF for whatever comes after.
  const [err] = await settle((cb) => fs.read(fd, Buffer.alloc(1), 0, 1, 0, cb));
  console.log("read after close", code(err));
}

async function fileHandle() {
  fs.writeFileSync(file, ALPHA);
  const fh = await fsp.open(file, "r+");
  const results = await Promise.allSettled([
    fh.read(Buffer.alloc(3), 0, 3, 0),
    fh.read(Buffer.alloc(3), 0, 3, 3),
    fh.read(Buffer.alloc(3), 0, 3, 6),
    fh.write(Buffer.from("xyz"), 0, 3, 23),
    fh.stat(),
  ]);
  for (const r of results) {
    if (r.status === "rejected") console.log("filehandle", r.reason.code);
    else if (typeof r.value.bytesWritten === "number") console.log("filehandle write", r.value.bytesWritten);
    else if (typeof r.value.bytesRead === "number") console.log("filehandle read", r.value.bytesRead, r.value.buffer.toString());
    else console.log("filehandle stat", typeof r.value.size);
  }
  await fh.close();
  console.log("file", fs.readFileSync(file, "utf8"));
}

await callbackForms();
await fileHandle();
fs.rmSync(dir, { recursive: true, force: true });
