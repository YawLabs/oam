// node-differential: an ASYNC fs rejection carries node's full system-error
// shape, not just `code`.
//
// Origin: OpOutcome::NodeFailed carried {code, message} only, so every
// promise-form fs failure rejected with `syscall`, `errno` and `path` all
// undefined while its sync twin (throw_node_error) set all four. Packages
// that branch on err.syscall === "open" or read err.path -- graceful-fs,
// chokidar, rimraf -- saw nothing there.
//
// The path SEPARATOR is deliberately not compared: oam echoes the caller's
// string where node normalizes to backslashes on win32. That divergence
// predates this case and applies equally to the sync path, so it is asserted
// as "a string naming the target" rather than byte-for-byte.
import fs from "node:fs";
import fsp from "node:fs/promises";

const MISSING = "./no-such-dir-here/no-such-file";
const shape = (e) => [
  e.code,
  e.syscall,
  e.errno,
  typeof e.path === "string" && /no-such-file$/.test(e.path.replace(/\\/g, "/")),
].join(" ");

// Sync and async must agree with each other AND with node.
try { fs.statSync(MISSING); console.log("statSync: NO THROW"); }
catch (e) { console.log("sync  stat    ", shape(e)); }
await fsp.stat(MISSING).then(() => console.log("stat: NO REJECT"), (e) => console.log("async stat    ", shape(e)));

try { fs.readFileSync(MISSING); console.log("readFileSync: NO THROW"); }
catch (e) { console.log("sync  readFile", shape(e)); }
await fsp.readFile(MISSING).then(() => console.log("readFile: NO REJECT"), (e) => console.log("async readFile", shape(e)));

for (const [label, run] of [
  ["unlink  ", () => fsp.unlink(MISSING)],
  ["readdir ", () => fsp.readdir(MISSING)],
  ["realpath", () => fsp.realpath(MISSING)],
  ["truncate", () => fsp.truncate(MISSING, 0)],
  ["copyFile", () => fsp.copyFile(MISSING, "./also-nowhere/x")],
]) {
  await run().then(
    () => console.log(`async ${label}: NO REJECT`),
    (e) => console.log(`async ${label}`, shape(e)),
  );
}

// An Error, and a plain one -- node does not use a subclass for these.
await fsp.stat(MISSING).catch((e) => {
  console.log("is Error", e instanceof Error, "| name", e.name);
  // errno is a NUMBER, not a string: code branches on `errno === -4058`.
  console.log("errno typeof", typeof e.errno, "| negative", e.errno < 0);
});
