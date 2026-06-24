// Classic synchronous fd ops: openSync/readSync/writeSync/closeSync/fstatSync.
// Regression guard for the gap where fs.openSync was undefined -- graceful-fs
// and @playwright/mcp probe a profile lock with `fs.openSync(path, "r+")` and
// branch on `err.code === "ENOENT"`, so a missing-file ENOENT is load-bearing.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

console.log("types", typeof fs.openSync, typeof fs.readSync, typeof fs.writeSync, typeof fs.closeSync, typeof fs.fstatSync);

// The lock-probe behavior: missing file in "r+" must throw ENOENT.
const missing = path.join(os.tmpdir(), "oam-fdsync-missing-" + Date.now(), "lockfile");
let code = "(no throw)";
try { fs.openSync(missing, "r+"); } catch (e) { code = e.code; }
console.log("missing-r+", code);

// write -> fstat -> read round-trip via fd.
const f = path.join(os.tmpdir(), "oam-conf-fdsync.txt");
const fd = fs.openSync(f, "w+");
console.log("fd", typeof fd);
const payload = Buffer.from("fd sync round-trip\n".repeat(4));
console.log("wrote", fs.writeSync(fd, payload, 0, payload.length, 0) === payload.length);
console.log("fstat", fs.fstatSync(fd).size === payload.length);
const back = Buffer.alloc(payload.length);
console.log("read", fs.readSync(fd, back, 0, back.length, 0) === payload.length, back.equals(payload));
fs.closeSync(fd);
let code2 = "(no throw)";
try { fs.fstatSync(fd); } catch (e) { code2 = e.code; }
console.log("closed-ebadf", code2);
fs.unlinkSync(f);
