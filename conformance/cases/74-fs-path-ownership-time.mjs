// Path-based ownership and time ops: chown, lchown, utimes, lutimes, lchmod --
// callback, sync and promise forms.
//
// Several behaviours here are counter-intuitive and were measured against node
// rather than reasoned about, because guessing produces something that looks
// right and is wrong:
//
//   - chown on Windows is a no-op that SUCCEEDS, even for a path that does not
//     exist. libuv never touches the path, so there is no ENOENT to report.
//     "Validate the path first" would be more defensible in the abstract and
//     would diverge.
//   - the error syscall is the SINGULAR `utime` / `lutime`, not the plural
//     spelling of the JS function.
//   - lchmod is not symmetric across the two modules. In node:fs the name is
//     bound to UNDEFINED off macOS; in fs/promises it is always a function that
//     rejects with ERR_METHOD_NOT_IMPLEMENTED. The name must exist either way,
//     because a builtin's ESM named exports are its own enumerable keys and a
//     missing one is a link-time SyntaxError.
import fs from "node:fs";
import fsp from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const file = path.join(os.tmpdir(), `oam-conf-pathops-${process.pid}.txt`);
const missing = `${file}.does-not-exist`;
fs.writeFileSync(file, "x");

// -1 in both fields means "leave owner and group alone" -- the only portable
// call, since a real change needs privileges and Windows has no uid/gid.
const KEEP = -1 >>> 0;
// A fixed UTC instant, so the assertion cannot drift with the clock or the
// machine's timezone.
const when = new Date(Date.UTC(2001, 0, 2, 3, 4, 5));

const show = (label, fn) => {
  try {
    const r = fn();
    console.log(label, "=> ok", r === undefined ? "" : r);
  } catch (e) {
    console.log(label, `=> ${e.code} syscall=${e.syscall} hasPath=${e.path !== undefined}`);
  }
};

// --- sync forms
show("chownSync           ", () => fs.chownSync(file, KEEP, KEEP));
show("lchownSync          ", () => fs.lchownSync(file, KEEP, KEEP));
show("utimesSync          ", () => fs.utimesSync(file, when, when));
show("  mtime             ", () => fs.statSync(file).mtime.toISOString());
show("lutimesSync         ", () => fs.lutimesSync(file, when, when));

// Time coercion: node takes Date, a number of SECONDS, or a numeric string.
show("utimesSync(seconds) ", () => fs.utimesSync(file, 1000000000, 1000000000));
show("  mtime             ", () => fs.statSync(file).mtime.toISOString());
show("utimesSync(string)  ", () => fs.utimesSync(file, "1000000000", "1000000000"));
show("  mtime             ", () => fs.statSync(file).mtime.toISOString());
show("utimesSync(bad)     ", () => fs.utimesSync(file, {}, {}));

// Directories work -- on Windows that needs FILE_FLAG_BACKUP_SEMANTICS to open
// the handle at all.
const dir = path.join(os.tmpdir(), `oam-conf-pathops-dir-${process.pid}`);
fs.mkdirSync(dir, { recursive: true });
show("utimesSync(dir)     ", () => fs.utimesSync(dir, when, when));
show("  dir mtime         ", () => fs.statSync(dir).mtime.toISOString());
fs.rmdirSync(dir);

// --- error shapes on a missing path
show("utimesSync(missing) ", () => fs.utimesSync(missing, when, when));
show("lutimesSync(missing)", () => fs.lutimesSync(missing, when, when));
show("chownSync(missing)  ", () => fs.chownSync(missing, KEEP, KEEP));

// --- callback forms
const call = (fn, ...args) =>
  new Promise((resolve, reject) => fn(...args, (err) => (err ? reject(err) : resolve())));

await call(fs.chown, file, KEEP, KEEP);
await call(fs.lchown, file, KEEP, KEEP);
await call(fs.utimes, file, when, when);
console.log("utimes cb mtime:", fs.statSync(file).mtime.toISOString());
await call(fs.lutimes, file, when, when);
console.log("callback forms: ok");

// A missing path reports through the callback, not at the call site.
let threwSynchronously = false;
let viaCallback = null;
try {
  await new Promise((resolve) => {
    fs.utimes(missing, when, when, (err) => {
      viaCallback = err;
      resolve();
    });
  });
} catch {
  threwSynchronously = true;
}
console.log("utimes(missing) threw synchronously:", threwSynchronously);
console.log("utimes(missing) callback:", viaCallback && viaCallback.code, viaCallback && viaCallback.syscall);

// --- promise forms
await fsp.chown(file, KEEP, KEEP);
await fsp.lchown(file, KEEP, KEEP);
await fsp.utimes(file, when, when);
await fsp.lutimes(file, when, when);
console.log("promises mtime:", fs.statSync(file).mtime.toISOString());

// --- lchmod, the asymmetric one
console.log("fs.lchmod type:", typeof fs.lchmod, "| own key:", Object.keys(fs).includes("lchmod"));
console.log(
  "fs.lchmodSync type:",
  typeof fs.lchmodSync,
  "| own key:",
  Object.keys(fs).includes("lchmodSync"),
);
console.log("fsp.lchmod type:", typeof fsp.lchmod);
if (process.platform !== "darwin") {
  try {
    await fsp.lchmod(file, 0o644);
    console.log("fsp.lchmod => resolved");
  } catch (e) {
    console.log(`fsp.lchmod => ${e.code} name=${e.name} keys=${JSON.stringify(Object.keys(e))}`);
    console.log("fsp.lchmod message:", e.message);
  }
}

fs.unlinkSync(file);
