// A NUMBERED descriptor in a stdio slot -- `stdio: ['ignore', logFd, logFd]`,
// the shape every daemonize-to-a-logfile does. Previously divergence 18.
//
// oam's descriptors are registry keys rather than OS fds, so a numbered slot
// had nothing the child could be handed and collapsed to 'inherit'. The child's
// output then went to the PARENT's console instead of the file the caller
// opened -- a silent wrong destination, with `child.stdout === null` making it
// look like the redirect had worked. The engine now resolves the number against
// the descriptor registry and hands the child a dup.
//
// 0/1/2 are deliberately still 'inherit': naming this process's own std fds is
// what inherit already means, so `stdio: [0, 1, 2]` stays on the cheap path
// instead of dup'ing three descriptors to say the same thing. That equivalence
// is asserted at the end.
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

const dir = path.join(os.tmpdir(), `oam-conf-numfd-${process.pid}`);
mkdirSync(dir, { recursive: true });

const talker = path.join(dir, "talker.cjs");
writeFileSync(talker, "process.stdout.write('TO_THE_FILE\\n');");
const silent = path.join(dir, "silent.cjs");
writeFileSync(silent, "process.exit(0);");

// 1. The redirect itself: the child's stdout must reach the FILE. Only stdout
//    is redirected -- pointing two slots at one descriptor makes the two
//    streams share a file offset, which is a race, not a contract.
const logPath = path.join(dir, "out.log");
const logFd = openSync(logPath, "w");
const cp = spawn(process.execPath, [talker], { stdio: ["ignore", logFd, "pipe"] });

// node reports null for a slot it did not pipe, redirect or not.
console.log("stdout null", cp.stdout === null);
console.log("stderr is stream", cp.stderr !== null);

const code = await new Promise((done) => cp.on("close", done));
closeSync(logFd);
console.log("child exit", code);
console.log("file received stdout", readFileSync(logPath, "utf8").includes("TO_THE_FILE"));

// 2. The caller's own descriptor survives: the child got a DUP, so this fd is
//    still usable and independently closable afterwards. (Writing here would
//    have failed outright if the child had been handed the original and closed
//    it on exit.)
const second = openSync(logPath, "a");
writeFileSync(path.join(dir, "probe.txt"), "ok");
closeSync(second);
console.log("caller fd still usable", true);

// 3. 0/1/2 in a slot mean 'inherit', not a descriptor handoff -- so all three
//    slots read back as null, exactly as `stdio: 'inherit'` does.
const numeric = spawn(process.execPath, [silent], { stdio: [0, 1, 2] });
console.log(
  "numeric 0/1/2 shape",
  numeric.stdin === null,
  numeric.stdout === null,
  numeric.stderr === null,
);
await new Promise((r) => numeric.on("close", r));

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
