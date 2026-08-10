// node-differential: fs.statfs / fs.Dir / the node:console named exports.
//
// Origin: `import { statfsSync } from "node:fs"` was a link-time SyntaxError
// on oam ("does not provide an export named 'statfsSync'"), which killed a
// bundled CLI that never called the function -- a builtin's ESM named exports
// are its module object's own enumerable keys, so an absent key fails the
// whole program at LINK time, not at the call site. The export-parity gate
// (conformance/surface-gaps.json) now guards the general case; this case
// pins the BEHAVIOUR of what was implemented to close it.
//
// Volume-dependent numbers (bfree/bavail) are never printed -- they drift
// between the two runs. Everything printed must be identical under both.
import { statfsSync, statfs, Dir, F_OK, X_OK, W_OK, R_OK } from "node:fs";
import fs from "node:fs";
import fsp from "node:fs/promises";
import { log, error, warn, Console } from "node:console";

const s = statfsSync(".");
console.log("ctor", s.constructor.name);
console.log("keys", JSON.stringify(Object.keys(s)));
console.log("proto", JSON.stringify(Object.getOwnPropertyNames(Object.getPrototypeOf(s))));
console.log("types", ["type", "bsize", "blocks", "bfree", "bavail", "files", "ffree"]
  .map((k) => typeof s[k]).join(","));
console.log("bsize>0", s.bsize > 0, "blocks>0", s.blocks > 0);
// StatFs stays internal in node -- exporting it would be a divergence.
console.log("StatFs exported", typeof fs.StatFs);

// A file path answers for the volume it lives on, same as a directory.
console.log("file-matches-dir", statfsSync("conformance/cases/70-fs-statfs-console-surface.mjs").blocks === s.blocks);

const big = statfsSync(".", { bigint: true });
console.log("bigint", big.constructor.name, typeof big.bsize, big.bsize === BigInt(s.bsize));

// Errors carry node's code/syscall on the sync path.
try {
  statfsSync("./no-such-directory-here/at-all");
  console.log("statfs missing path: NO THROW");
} catch (e) {
  console.log("statfs err", e.code, e.syscall, /statfs/.test(e.message));
}

await new Promise((resolve) => {
  statfs(".", (err, value) => {
    console.log("cb", err, value.constructor.name, value.bsize === s.bsize);
    resolve();
  });
});
const viaPromise = await fsp.statfs(".");
console.log("promise", viaPromise.constructor.name, viaPromise.bsize === s.bsize);
// options AND callback together -- the one arity the callback wrapper has to
// split by hand (it pops the trailing callback and forwards the rest).
await new Promise((resolve) => {
  statfs(".", { bigint: true }, (err, value) => {
    console.log("cb+options", err, typeof value.bsize, value.bsize === BigInt(s.bsize));
    resolve();
  });
});

// ------------------------------------------------------------------- fs.Dir
console.log("access consts", F_OK, X_OK, W_OK, R_OK);
console.log("Dir", typeof Dir);
const dir = fs.opendirSync("conformance/cases");
console.log("opendirSync instanceof Dir", dir instanceof Dir);
console.log("Dir methods", ["read", "readSync", "close", "closeSync"].map((m) => typeof dir[m]).join(","));
const entry = dir.readSync();
console.log("readSync name is string", typeof entry.name === "string", "isFile", entry.isFile());
dir.closeSync();
// A closed handle throws on every later operation -- it does not report
// end-of-directory, which would make a use-after-close look like an empty dir.
try { dir.readSync(); console.log("closed readSync: NO THROW"); }
catch (e) { console.log("closed readSync", e.code); }
try { dir.closeSync(); console.log("double close: NO THROW"); }
catch (e) { console.log("double close", e.code); }
await dir.read().then(() => console.log("closed read: NO REJECT"), (e) => console.log("closed read", e.code));

const adir = await fsp.opendir("conformance/cases");
console.log("opendir instanceof Dir", adir instanceof Dir);
let count = 0;
for await (const _e of adir) count++;
console.log("async iteration saw entries", count > 0);
// Iterating to the end closes the handle, so close() afterwards must reject.
await adir.close().then(() => console.log("close after iteration: NO REJECT"),
  (e) => console.log("close after iteration", e.code));
// ...and so does abandoning the loop early.
const bdir = await fsp.opendir("conformance/cases");
for await (const _e of bdir) break;
await bdir.close().then(() => console.log("close after break: NO REJECT"),
  (e) => console.log("close after break", e.code));

// Argument validation matches node's TYPE and message, not just the code.
try { fs.watchFile("conformance/cases"); console.log("watchFile no listener: NO THROW"); }
catch (e) { console.log("watchFile no listener", e.name, e.code, e.message); }
try { fs._toUnixTimestamp({}); console.log("_toUnixTimestamp obj: NO THROW"); }
catch (e) { console.log("_toUnixTimestamp obj", e.name, e.code, e.message); }
// The string branch of the "Received ..." tail quotes the value.
try { fs._toUnixTimestamp("abc"); console.log("_toUnixTimestamp str: NO THROW"); }
catch (e) { console.log("_toUnixTimestamp str", e.name, e.code, e.message); }
// A NEGATIVE time means "now", not a pre-epoch date. The value is clock-
// dependent, so assert the property rather than the number.
const nowish = fs._toUnixTimestamp(-1);
console.log("_toUnixTimestamp negative is now", Math.abs(nowish - Date.now() / 1000) < 5);
try { statfsSync(""); console.log("statfs empty: NO THROW"); }
catch (e) { console.log("statfs empty", e.code, e.syscall); }

console.log("_toUnixTimestamp", fs._toUnixTimestamp(new Date(5000)), fs._toUnixTimestamp("12"), fs._toUnixTimestamp(7));
console.log("unwatchFile", typeof fs.unwatchFile, "fstat", typeof fs.fstat);

const fd = fs.openSync("conformance/cases/70-fs-statfs-console-surface.mjs", "r");
await new Promise((resolve) => {
  fs.fstat(fd, (err, st) => {
    console.log("fstat", err, st.isFile(), st.size > 0);
    resolve();
  });
});
fs.closeSync(fd);
// A bad fd reports through the CALLBACK. It must not throw synchronously --
// promisify(fs.fstat) would otherwise throw straight past the caller's catch.
let fstatThrewSync = false;
const badFdErr = await new Promise((resolve) => {
  try { fs.fstat(1000000, (err) => resolve(err)); }
  catch (e) { fstatThrewSync = true; resolve(e); }
});
console.log("fstat bad fd", badFdErr && badFdErr.code, "threw sync:", fstatThrewSync);
// Same path via the closed fd above.
await new Promise((resolve) => fs.fstat(fd, (err) => {
  console.log("fstat closed fd", err && err.code);
  resolve();
}));

// Dir's CALLBACK forms (node's read/close are dual-shaped: promise or cb).
const cbdir = fs.opendirSync("conformance/cases");
await new Promise((resolve) => cbdir.read((err, ent) => {
  console.log("Dir read(cb)", err, typeof ent.name === "string");
  resolve();
}));
await new Promise((resolve) => cbdir.close((err) => {
  console.log("Dir close(cb)", err);
  resolve();
}));
// A closed handle throws OUT of read(cb) rather than reporting through it --
// node runs the guard inline when a callback is supplied. close(cb) is the
// other way round and reports through the callback.
try { cbdir.read(() => console.log("Dir read(cb) after close: CALLED BACK")); }
catch (e) { console.log("Dir read(cb) after close: SYNC THROW", e.code); }
await new Promise((resolve) => cbdir.close((err) => {
  console.log("Dir close(cb) after close", err && err.code);
  resolve();
}));

// --------------------------------------------------------------- node:console
// The module object IS globalThis.console in node; it used to be an
// Object.create() of it here, which left every method on the prototype and
// so exported nothing but Console.
console.log("console named", [log, error, warn, Console].map((v) => typeof v).join(","));
const consoleMod = (await import("node:console")).default;
console.log("console identity", consoleMod === globalThis.console);
console.log("console has Console own", Object.keys(consoleMod).includes("Console"));
console.log("extras", ["dirxml", "profile", "profileEnd", "timeStamp", "createTask", "context"]
  .map((k) => typeof consoleMod[k]).join(","));
console.log("createTask run", consoleMod.createTask("t").run(() => 42));
console.log("dirxml is not log", consoleMod.dirxml !== consoleMod.log);
console.log("context has log", typeof consoleMod.context("c").log);
