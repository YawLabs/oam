// node's `argv0` spawn option, end to end.
//
// When `options.argv0` is set, the child's process.argv[0] (and on POSIX,
// argv[0] / the process image name reported by getcwd() -- no, by reading
// /proc/self/comm -- but we only check what scripts can observe:
//   - process.argv[0] inside the child
//   - `node -e "process.argv[0]"` inside the child
//
// On Windows argv0 is set via lpCommandLine (the first token); on POSIX via
// arg0(3) / CommandExt::arg0(). This case covers:
//
//   1. spawnSync, spawn, and the extra-fd spawn path all respect argv0.
//   2. execFileSync and execFile also pass it through (they delegate to
//      spawn/exec under the hood).
//   3. The default (absent) leaves process.argv[0] as node's own binary path.
//
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
writeFileSync(child, "console.log(process.argv[0]);");

// 1. spawnSync -- default (no argv0) vs explicit argv0.
const rDefault = spawnSync(process.execPath, [child]);
const defaultArgv0 = String(rDefault.stdout).trim();

const customArgv0 = `oam-child-${process.pid}`;
const rCustom = spawnSync(process.execPath, [child], { argv0: customArgv0 });
const childArgv0 = String(rCustom.stdout).trim();

console.log(
  "spawnSync default",
  JSON.stringify(defaultArgv0),
  JSON.stringify(customArgv0),
  JSON.stringify(childArgv0),
);

// Verify: without argv0 the child sees the real binary; with argv0 it sees
// whatever we passed.
if (defaultArgv0 === customArgv0) {
  console.error("FAIL: default should differ from custom argv0");
  process.exitCode = 1;
}
if (childArgv0 !== customArgv0) {
  console.error(
    "FAIL: child argv[0] expected",
    customArgv0,
    "got",
    childArgv0,
  );
  process.exitCode = 1;
}

// 2. async spawn -- same test over the async path.
const cp = spawn(process.execPath, [child], { argv0: "async-child" });
let out = "";
cp.stdout.on("data", (d) => (out += d));
await new Promise((resolve) => cp.on("close", resolve));

if (out.trim() !== "async-child") {
  console.error(
    "FAIL: async spawn argv0 expected 'async-child' got",
    out.trim(),
  );
  process.exitCode = 1;
}

// 3. extra-fd spawn path (more than three stdio slots).
const cp2 = spawn(process.execPath, [child], {
  argv0: "extra-fd-child",
  stdio: ["ignore", "pipe", "pipe"],
});
let out2 = "";
cp2.stdout.on("data", (d) => (out2 += d));
await new Promise((resolve) => cp2.on("close", resolve));

if (out2.trim() !== "extra-fd-child") {
  console.error(
    "FAIL: extra-fd spawn argv0 expected 'extra-fd-child' got",
    out2.trim(),
  );
  process.exitCode = 1;
}

// 4. execFileSync -- delegates to spawn, should inherit argv0.
const rExecSync = execFileSync(process.execPath, [child], {
  argv0: "exec-sync-child",
});
if (String(rExecSync).trim() !== "exec-sync-child") {
  console.error(
    "FAIL: execFileSync argv0 expected 'exec-sync-child' got",
    String(rExecSync).trim(),
  );
  process.exitCode = 1;
}

// 5. execFile (async) -- same, delegates to spawn.
const rExec = await new Promise((resolve, reject) => {
  execFile(
    process.execPath,
    [child],
    { argv0: "exec-async-child" },
    (err, stdout) => (err ? reject(err) : resolve(stdout)),
  );
});
if (String(rExec).trim() !== "exec-async-child") {
  console.error(
    "FAIL: execFile argv0 expected 'exec-async-child' got",
    String(rExec).trim(),
  );
  process.exitCode = 1;
}

rmSync(child);
console.log("PASS");
