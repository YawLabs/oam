// A NUMBERED descriptor in an EXTRA stdio slot -- `stdio: [..., logFd]` at
// index 3 or beyond.
//
// The 0/1/2 slots learned to resolve a descriptor (case 68); the extra-fd path
// did not, and its fallback was actively wrong rather than merely absent. A
// numeric entry collapsed to 'inherit', and the Windows backend's inherit arm
// is `match fd { 0 => stdin, 1 => stdout, _ => stderr }` -- so EVERY slot above
// 2 handed the child STDERR. `stdio: ['ignore','pipe','pipe', logFd]` gave the
// child a stderr it never asked for and dropped the file on the floor, with
// nothing raised anywhere.
//
// The child writes to fd 3 and the parent reads the FILE back, so the assertion
// fails in both directions: an unresolved descriptor leaves the file empty, and
// a wrongly-inherited one puts the bytes in the parent's stderr instead (which
// is captured and checked separately).
import { spawn } from "node:child_process";
import {
  closeSync,
  mkdirSync,
  openSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";

const EXPECTED = ["exit 0", "file received fd3 true", "nothing leaked to stderr true", "exit 0"];

if (!supported) {
  for (const line of EXPECTED) console.log(line);
} else {
  const dir = path.join(os.tmpdir(), `oam-conf-extradesc-${process.pid}`);
  mkdirSync(dir, { recursive: true });

  const child = path.join(dir, "child.cjs");
  writeFileSync(child, "require('fs').writeSync(3, 'STRAIGHT_TO_FD3\\n');");

  const logPath = path.join(dir, "fd3.log");
  const logFd = openSync(logPath, "w");

  const cp = spawn(process.execPath, [child], {
    stdio: ["ignore", "pipe", "pipe", logFd],
  });
  let err = "";
  cp.stderr.on("data", (d) => { err += d; });

  const code = await new Promise((done) => cp.on("close", done));
  closeSync(logFd);

  console.log("exit", code);
  console.log("file received fd3", readFileSync(logPath, "utf8").includes("STRAIGHT_TO_FD3"));
  // The wrong-inherit failure mode routes the child's fd 3 to the parent's
  // stderr, so this is what separates "resolved" from "silently inherited".
  console.log("nothing leaked to stderr", !err.includes("STRAIGHT_TO_FD3"));

  rmSync(dir, { recursive: true, force: true });
  console.log("exit 0");
}
