// fd-based fs ops: fsync / fdatasync / ftruncate / fchmod / fchown / futimes,
// in both the callback and the sync form.
//
// Every one of these was a missing named export, and a builtin's ESM named
// exports are its module object's own enumerable keys -- so importing one oam
// lacked was a LINK-time SyntaxError that killed the whole program, even for a
// program that never called it.
//
// The behaviours worth pinning, beyond "the export exists":
//   - ftruncate actually truncates, and tolerates the omitted-length form
//   - futimes lands the exact time it was given (libc futimens on unix,
//     SetFileTime on Windows, via two different epoch conversions)
//   - a bad descriptor reports EBADF THROUGH THE CALLBACK, not synchronously,
//     which is where node reports it and where a naive callbackify would not
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const file = path.join(os.tmpdir(), `oam-conf-fdops-${process.pid}`);
fs.writeFileSync(file, "hello world");
const fd = fs.openSync(file, "r+");

// --- sync forms
fs.fsyncSync(fd);
fs.fdatasyncSync(fd);
fs.ftruncateSync(fd, 5);
console.log("ftruncateSync:", JSON.stringify(fs.readFileSync(file, "utf8")));

// A fixed UTC instant so the assertion cannot drift with the clock or the
// machine's timezone.
const when = new Date(Date.UTC(2001, 0, 2, 3, 4, 5));
fs.futimesSync(fd, when, when);
console.log("futimesSync mtime:", fs.fstatSync(fd).mtime.toISOString());

fs.fchmodSync(fd, 0o644);
// -1 in both fields means "leave owner and group alone" -- the only portable
// call, since a real uid/gid change needs privileges and Windows has neither.
fs.fchownSync(fd, -1 >>> 0, -1 >>> 0);
console.log("sync forms: ok");

// --- callback forms
const call = (fn, ...args) =>
  new Promise((resolve, reject) => fn(...args, (err) => (err ? reject(err) : resolve())));

await call(fs.fsync, fd);
await call(fs.fdatasync, fd);
await call(fs.ftruncate, fd, 2);
console.log("ftruncate:", JSON.stringify(fs.readFileSync(file, "utf8")));
await call(fs.futimes, fd, when, when);
console.log("futimes mtime:", fs.fstatSync(fd).mtime.toISOString());
await call(fs.fchmod, fd, 0o644);
console.log("callback forms: ok");

// The omitted-length form: node treats a missing length as 0.
await call(fs.ftruncate, fd);
console.log("ftruncate default length:", fs.fstatSync(fd).size);

// --- a closed descriptor reports through the callback, never at the call site
fs.closeSync(fd);
let threwSynchronously = false;
let viaCallback = null;
try {
  await new Promise((resolve) => {
    fs.fsync(fd, (err) => {
      viaCallback = err;
      resolve();
    });
  });
} catch {
  threwSynchronously = true;
}
console.log("closed fd threw synchronously:", threwSynchronously);
console.log("closed fd callback code:", viaCallback && viaCallback.code);

// The sync form is the opposite: it throws, and carries the same code.
let syncCode = null;
try {
  fs.fsyncSync(fd);
} catch (e) {
  syncCode = e.code;
}
console.log("closed fd sync code:", syncCode);

fs.unlinkSync(file);
