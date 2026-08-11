// Positional read/write must not move the descriptor's cursor -- pread(2) /
// pwrite(2) semantics -- and fs error objects must enumerate their own
// properties in node's order.
//
// Both were real divergences in shipped oam:
//
//   readSync(fd, b, 0, 3, 10) then readSync(fd, b, 0, 3, null)
//     node: "KLM" then "ABC"   (the sequential read still starts at 0)
//     oam:  "KLM" then "NOP"   (the positional read had moved the cursor)
//
// That is the worst shape a bug can have here: no error is raised anywhere, the
// caller simply gets different bytes than node would have given it. The write
// side had the identical defect.
//
// Object.keys(err) is observable, so property ORDER is part of the error shape
// -- anything that snapshots or diffs an error object sees it. node enumerates
// errno, code, syscall (errno first, despite being the only non-string).
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const file = path.join(os.tmpdir(), `oam-conf-pos-${process.pid}.txt`);
fs.writeFileSync(file, "ABCDEFGHIJKLMNOPQRSTUVWXYZ");

// --- positional read leaves the cursor alone
{
  const fd = fs.openSync(file, "r");
  const a = Buffer.alloc(3);
  const b = Buffer.alloc(3);
  fs.readSync(fd, a, 0, 3, 10);
  fs.readSync(fd, b, 0, 3, null);
  console.log("read(pos=10):", a.toString(), "| then read(null):", b.toString());

  // A second positional read is also independent of the cursor.
  const c = Buffer.alloc(3);
  fs.readSync(fd, c, 0, 3, 20);
  const d = Buffer.alloc(3);
  fs.readSync(fd, d, 0, 3, null);
  console.log("read(pos=20):", c.toString(), "| then read(null):", d.toString());
  fs.closeSync(fd);
}

// --- positional write leaves the cursor alone
{
  const p = `${file}.w`;
  fs.writeFileSync(p, "0123456789");
  const fd = fs.openSync(p, "r+");
  fs.writeSync(fd, Buffer.from("XX"), 0, 2, 4);
  fs.writeSync(fd, Buffer.from("Y"), 0, 1);
  fs.closeSync(fd);
  console.log("write(pos=4) then write(no pos):", fs.readFileSync(p, "utf8"));
  fs.unlinkSync(p);
}

// --- sequential reads still advance the cursor when no position is given
{
  const fd = fs.openSync(file, "r");
  const out = [];
  for (let i = 0; i < 3; i++) {
    const b = Buffer.alloc(4);
    fs.readSync(fd, b, 0, 4, null);
    out.push(b.toString());
  }
  console.log("sequential reads:", JSON.stringify(out));
  fs.closeSync(fd);
}

// --- error own-property order and values
{
  const fd = fs.openSync(file, "r");
  fs.closeSync(fd);
  try {
    fs.readSync(fd, Buffer.alloc(4), 0, 4, 0);
  } catch (e) {
    console.log("closed fd own props:", JSON.stringify(Object.keys(e)));
    console.log("closed fd code/syscall:", e.code, e.syscall);
    console.log("closed fd message:", e.message);
  }
}

// A path-bearing error carries path too, still errno-first.
try {
  fs.statSync(`${file}.missing`);
} catch (e) {
  console.log("missing path own props:", JSON.stringify(Object.keys(e)));
}

fs.unlinkSync(file);
