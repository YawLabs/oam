// node:module's compile-cache API, node v22.22.2's shape: the four names
// (default and named exports alike), the frozen null-prototype status bag,
// getCompileCacheDir() undefined until enabled, enableCompileCache(dir)
// answering ENABLED with the resolved directory and then ALREADY_ENABLED
// with the per-build subdirectory it created inside it, node's own
// TypeError for a non-string directory (null included), FAILED with libuv's
// wording when the directory cannot be created, and flushCompileCache()
// returning undefined. Then the environment: NODE_COMPILE_CACHE enables it
// before the program runs (ALREADY_ENABLED from the first call, relative
// values resolved against the cwd) and NODE_DISABLE_COMPILE_CACHE answers
// DISABLED for any value, "0" included.
//
// Regression guard: oam's node:module had none of these, so
// `module.enableCompileCache()` threw "is not a function" and importing the
// names was a link-time SyntaxError.
import mod, { constants, enableCompileCache, flushCompileCache, getCompileCacheDir } from "node:module";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "cc438-"));
// The per-build subdirectory's name is runtime-specific (node's is
// v<version>-<arch>-<hash>, oam's oam-v<version>-<arch>-<v8 tag>); show
// where it sits, never its spelling.
const named = new Set(["cache", "other"]);
const where = (dir) => {
  if (dir === undefined) return "undefined";
  const rel = path.relative(scratch, dir);
  if (rel.startsWith("..") || path.isAbsolute(rel)) return "<elsewhere>";
  return ["<scratch>", ...rel.split(path.sep).filter(Boolean).map((seg) => (named.has(seg) ? seg : "<version dir>"))].join("/");
};
const show = (label, r) =>
  console.log(label, JSON.stringify(Object.keys(r)), r.status, r.message ?? "-", where(r.directory));

console.log(
  [mod.enableCompileCache, mod.getCompileCacheDir, mod.flushCompileCache]
    .map((f) => `${typeof f}:${f.name}/${f.length}`)
    .join(" "),
);
console.log(
  enableCompileCache === mod.enableCompileCache,
  getCompileCacheDir === mod.getCompileCacheDir,
  flushCompileCache === mod.flushCompileCache,
  constants === mod.constants,
);
const s = constants.compileCacheStatus;
console.log(JSON.stringify(s), Object.getPrototypeOf(constants), Object.getPrototypeOf(s));
console.log(Object.isFrozen(constants), Object.isFrozen(s), JSON.stringify(Object.keys(constants)));
const d = Object.getOwnPropertyDescriptor(mod, "constants");
console.log(d.writable, d.enumerable, d.configurable);

console.log("before", getCompileCacheDir(), flushCompileCache());
for (const bad of [123, null, { directory: scratch }, true]) {
  try {
    enableCompileCache(bad);
    console.log("no throw", bad);
  } catch (e) {
    console.log(e.constructor.name, e.code, e.message, JSON.stringify(Object.keys(e)));
  }
}
console.log("after bad", getCompileCacheDir());

fs.writeFileSync(path.join(scratch, "file"), "x");
show("under a file", enableCompileCache(path.join(scratch, "file", "sub")));
console.log("after failed", getCompileCacheDir());

const first = enableCompileCache(path.join(scratch, "cache"));
show("first", first);
console.log(first.directory === path.join(scratch, "cache"));
console.log("dir", where(getCompileCacheDir()));
console.log("made", fs.statSync(getCompileCacheDir()).isDirectory());
show("again", enableCompileCache(path.join(scratch, "other")));
show("no arg", enableCompileCache());
console.log(enableCompileCache().directory === getCompileCacheDir());
console.log("other made", fs.existsSync(path.join(scratch, "other")));
console.log("flush", flushCompileCache());

// The environment, in fresh processes.
const probeFile = path.join(scratch, "probe.mjs");
const child = (env) => {
  // OAM_CACHE_DIR (a harness's isolation) stays: NODE_COMPILE_CACHE
  // outranks it. oam's own off switch would answer DISABLED, so it goes.
  const e = { ...process.env, ...env };
  delete e.OAM_CODE_CACHE;
  if (!("NODE_DISABLE_COMPILE_CACHE" in env)) delete e.NODE_DISABLE_COMPILE_CACHE;
  if (!("NODE_COMPILE_CACHE" in env)) delete e.NODE_COMPILE_CACHE;
  return execFileSync(process.execPath, [probeFile], { env: e, cwd: scratch, encoding: "utf8" });
};
fs.writeFileSync(probeFile, `
import m from "node:module";
import path from "node:path";
const before = m.getCompileCacheDir();
const r = m.enableCompileCache(path.join(process.cwd(), "late"));
console.log(JSON.stringify([before === undefined ? "undefined" : path.relative(process.cwd(), path.dirname(before)), r.status, r.message ?? "-", r.directory === before, m.getCompileCacheDir() === before]));
`);
process.stdout.write(child({ NODE_COMPILE_CACHE: path.join(scratch, "env") }));
process.stdout.write(child({ NODE_COMPILE_CACHE: "relative" }));
process.stdout.write(child({ NODE_DISABLE_COMPILE_CACHE: "1" }));
process.stdout.write(child({ NODE_DISABLE_COMPILE_CACHE: "0" }));
process.stdout.write(child({ NODE_DISABLE_COMPILE_CACHE: "1", NODE_COMPILE_CACHE: path.join(scratch, "env2") }));
console.log("disabled made nothing", fs.existsSync(path.join(scratch, "env2")), fs.existsSync(path.join(scratch, "late")));
console.log("env made", fs.existsSync(path.join(scratch, "env")), fs.existsSync(path.join(scratch, "relative")));

fs.rmSync(scratch, { recursive: true, force: true });
