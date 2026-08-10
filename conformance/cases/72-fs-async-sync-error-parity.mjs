// node-differential: for EVERY fs op that has both forms, the async
// rejection and the sync throw carry the same error shape.
//
// Case 71 pins the specific ops that were fixed; this one is the general
// rule, so a NEW async op that ships with a bare `code` (the state every
// promise-form op was in before) fails here rather than in a user's
// exception handler. Both runtimes must agree line for line, which also
// means the sync/async pairing itself is checked against node's.
//
// Separators are normalised: oam echoes the caller's path where node
// rewrites to backslashes on win32 -- a pre-existing divergence on both
// forms, not an async-vs-sync one.
import fs from "node:fs";
import fsp from "node:fs/promises";

const MISSING = "./no-such-dir-here/no-such-file";
const OTHER = "./no-such-dir-here/other";

// [label, sync call, async call] -- every pair oam implements on both sides.
const PAIRS = [
  ["stat", () => fs.statSync(MISSING), () => fsp.stat(MISSING)],
  ["lstat", () => fs.lstatSync(MISSING), () => fsp.lstat(MISSING)],
  ["readFile", () => fs.readFileSync(MISSING), () => fsp.readFile(MISSING)],
  ["readdir", () => fs.readdirSync(MISSING), () => fsp.readdir(MISSING)],
  ["unlink", () => fs.unlinkSync(MISSING), () => fsp.unlink(MISSING)],
  ["rmdir", () => fs.rmdirSync(MISSING), () => fsp.rmdir(MISSING)],
  // realpath and opendir are DELIBERATELY absent: node's own sync and async
  // forms disagree on them (realpathSync reports the `lstat` its path-walk
  // failed on where realpath() reports `realpath`; opendirSync omits `path`
  // where opendir() sets it). Asserting the rule on those would mean encoding
  // node's inconsistency into a case whose whole point is the rule. oam's
  // divergence there is recorded in docs/node-divergences.md instead.
  ["truncate", () => fs.truncateSync(MISSING, 0), () => fsp.truncate(MISSING, 0)],
  ["chmod", () => fs.chmodSync(MISSING, 0o644), () => fsp.chmod(MISSING, 0o644)],
  ["readlink", () => fs.readlinkSync(MISSING), () => fsp.readlink(MISSING)],
  ["rename", () => fs.renameSync(MISSING, OTHER), () => fsp.rename(MISSING, OTHER)],
  ["copyFile", () => fs.copyFileSync(MISSING, OTHER), () => fsp.copyFile(MISSING, OTHER)],
  ["link", () => fs.linkSync(MISSING, OTHER), () => fsp.link(MISSING, OTHER)],
  ["statfs", () => fs.statfsSync(MISSING), () => fsp.statfs(MISSING)],
  ["access", () => fs.accessSync(MISSING), () => fsp.access(MISSING)],
];

const shape = (e) =>
  e === null
    ? "NO ERROR"
    : [
        e.code,
        e.syscall,
        e.errno,
        typeof e.path === "string" ? "path" : String(e.path),
      ].join(" ");

const capture = (fn) => {
  try { fn(); return null; } catch (e) { return e; }
};
const captureAsync = async (fn) => {
  try { await fn(); return null; } catch (e) { return e; }
};

for (const [label, syncFn, asyncFn] of PAIRS) {
  const s = shape(capture(syncFn));
  const a = shape(await captureAsync(asyncFn));
  // Printing BOTH plus the verdict: when they disagree the diff shows which
  // side drifted, rather than just "false".
  console.log(`${label.padEnd(9)} sync[${s}] async[${a}] match=${s === a}`);
}

// A WRONG-TYPE failure rather than a missing one: rmdir on a regular file.
// oam decides this itself (it kind-checks before removing) instead of reading
// it off a syscall, and that hand-built error carried `code` alone. Every
// case above goes through the native error path, so none of them reached it.
const SELF = "conformance/cases/72-fs-async-sync-error-parity.mjs";
console.log("rmdir-on-file sync ", shape(capture(() => fs.rmdirSync(SELF))));
console.log("rmdir-on-file async", shape(await captureAsync(() => fsp.rmdir(SELF))));
