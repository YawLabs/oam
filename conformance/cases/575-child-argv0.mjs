// node's `argv0` spawn option, end to end.
//
// node sets the child's RAW command-line first token to `options.argv0`: it
// unshifts argv0 onto the child's argv (lib/child_process.js
// normalizeSpawnArguments, after any `shell` rewrite), and the program it runs
// is still `file`. What the child then observes:
//
//   - process.argv0 === the argv0 token (node captures argv0 from the OS
//     command line before overwriting argv[0]);
//   - process.argv[0] === process.execPath -- node overwrites argv[0] with the
//     resolved execPath on EVERY platform, so the two stay independent. A spawn
//     with argv0 does NOT change argv[0].
//
// Spawned WITHOUT argv0 the first token is the program as invoked, so
// process.argv0 is that string -- execPath when the call passes an absolute
// path -- while process.argv[0] is still execPath.
//
// Every value printed here is a runtime-INDEPENDENT relation (a token the call
// chose, or a boolean against process.execPath), never an absolute path:
// node's and oam's execPath differ, so printing them would diverge on the path
// alone. This case covers:
//
//   1. spawnSync, spawn, and an explicit-stdio spawn all set process.argv0.
//   2. execFileSync keeps argv0 (it passes options straight to spawnSync).
//   3. execFile DROPS argv0 -- node rebuilds the spawn options without it -- so
//      its child sees execPath, not the token.
import {
  execFile,
  execFileSync,
  spawn,
  spawnSync,
} from "node:child_process";
import { rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = os.tmpdir();
const child = path.join(dir, `oam-conf-575-${process.pid}.cjs`);
// The child reports its own argv0 and whether its argv[0] is its execPath --
// never the path itself.
writeFileSync(
  child,
  "process.stdout.write(JSON.stringify({" +
    "a0: process.argv0, firstIsExec: process.argv[0] === process.execPath }));",
);

let ok = true;
// `expectedArgv0` is the token the caller chose (or process.execPath when none
// was given, which is what argv0 reports then).
function report(label, out, expectedArgv0) {
  let o = {};
  try {
    o = JSON.parse(String(out).trim());
  } catch {
    /* leave o empty: both relations read false and the line is a divergence */
  }
  const argv0Matches = o.a0 === expectedArgv0;
  const firstIsExec = o.firstIsExec === true;
  console.log(label, "argv0-matches", argv0Matches, "argv0-is-execPath", firstIsExec);
  if (!argv0Matches || !firstIsExec) ok = false;
}

// 1. spawnSync -- explicit argv0, then the default (argv0 is execPath).
report(
  "spawnSync argv0  ",
  spawnSync(process.execPath, [child], { argv0: "sync-child" }).stdout,
  "sync-child",
);
report(
  "spawnSync default",
  spawnSync(process.execPath, [child]).stdout,
  process.execPath,
);

// 2. async spawn -- same over the async path.
const cp = spawn(process.execPath, [child], { argv0: "async-child" });
let out = "";
cp.stdout.on("data", (d) => (out += d));
await new Promise((resolve) => cp.on("close", resolve));
report("async spawn argv0", out, "async-child");

// 3. explicit stdio (an 'ignore' stdin beside piped stdout/stderr).
const cp2 = spawn(process.execPath, [child], {
  argv0: "extra-fd-child",
  stdio: ["ignore", "pipe", "pipe"],
});
let out2 = "";
cp2.stdout.on("data", (d) => (out2 += d));
await new Promise((resolve) => cp2.on("close", resolve));
report("stdio spawn argv0", out2, "extra-fd-child");

// 4. execFileSync keeps argv0 (options pass straight through to spawnSync).
report(
  "execFileSync argv0",
  execFileSync(process.execPath, [child], { argv0: "exec-sync-child" }),
  "exec-sync-child",
);

// 5. execFile (async) DROPS argv0: node rebuilds the options it hands spawn
//    without it, so the child sees its execPath, not the token.
const execOut = await new Promise((resolve, reject) => {
  execFile(
    process.execPath,
    [child],
    { argv0: "exec-async-child" },
    (err, stdout) => (err ? reject(err) : resolve(stdout)),
  );
});
report("execFile drop argv0", execOut, process.execPath);

rmSync(child);
console.log(ok ? "PASS" : "FAIL");
process.exitCode = ok ? 0 : 1;
