// process.abort() ends the process at once: no 'exit' listener runs, what was
// already written to stdout stays written, and the process dies -- exit code
// 134 on Windows, SIGABRT on POSIX (node v22.22.2, measured on Windows).
//
// Regression guard: oam's process.abort called natives.processExit, a native
// that was never registered, so abort() threw a TypeError, the 'exit'
// listeners ran, and the run exited 1 as an uncaught exception.
//
// oam has no abort primitive, so on POSIX it exits 134 where node dies of
// SIGABRT (docs/node-divergences.md #51). The POSIX leg therefore prints
// whether the child died of the abort in either form, not which form; the
// Windows leg prints the exact status. stderr is not compared: node prints a
// native and a JavaScript stack trace there, oam prints nothing.
import { spawnSync } from "node:child_process";

const child = spawnSync(
  process.execPath,
  [
    "-e",
    "process.on('exit', () => console.log('exit listener ran'));" +
      "console.log('before abort');" +
      "process.abort();" +
      "console.log('after abort');",
  ],
  { encoding: "utf8" },
);

console.log("stdout:", JSON.stringify(child.stdout.replace(/\r\n/g, "\n")));
if (process.platform === "win32") {
  console.log("status:", child.status, "signal:", child.signal);
} else {
  console.log("died of the abort:", child.status === 134 || child.signal === "SIGABRT");
}
