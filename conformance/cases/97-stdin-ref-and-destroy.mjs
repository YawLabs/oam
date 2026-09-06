// What keeps a process alive while stdin is a pipe nobody has closed.
//
// Each shape runs as a child on THIS runtime with its stdin piped and held
// open; the child either stops wanting stdin (exits promptly) or waits for
// the pipe to close. node's rules, which oam now matches:
//
// - pause() does NOT release the process. The handle stays open and node
//   keeps running -- the common belief that pause() lets the process exit is
//   wrong, and oam agreed here already.
// - unref() DOES: it is the documented way to stop stdin holding the process
//   open. oam had no ref/unref on stdin at all, so calling it threw
//   TypeError and killed the program (exit 1).
// - destroy() does, and so does `for await (...) break`, which destroys the
//   stream on the way out (destroyOnReturn). oam kept the pending read
//   counted, so the loop sat there until the pipe closed.
//
// The blocking read underneath cannot be cancelled, so retiring it means
// dropping it from the event loop's count; the read still completes if data
// arrives later.
import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-stdin-ref-${process.pid}`);
mkdirSync(dir, { recursive: true });

// How long the parent holds the pipe open, and the line between "released
// stdin" and "waited for the pipe". Wide enough that a loaded box cannot
// flip a verdict: a released child exits in tens of milliseconds, a waiting
// one at HOLD_MS.
const HOLD_MS = 2500;
const RELEASED_MS = 1200;

const shapes = [
  ["data + pause()", "process.stdin.on('data', () => process.stdin.pause());"],
  [
    "data + unref()",
    "process.stdin.on('data', () => { process.stdin.pause(); process.stdin.unref(); });",
  ],
  ["data + destroy()", "process.stdin.on('data', () => process.stdin.destroy());"],
  ["for await + break", "for await (const c of process.stdin) break;"],
  ["never reads stdin", "process.stdout.write('');"],
];

for (const [name, body] of shapes) {
  const file = path.join(dir, "child.mjs");
  writeFileSync(file, `${body}\n`);
  const started = Date.now();
  const child = spawn(process.execPath, [file], { stdio: ["pipe", "ignore", "inherit"] });
  child.stdin.write("x\n");
  const hold = setTimeout(() => {
    try {
      child.stdin.end();
    } catch {
      /* already gone */
    }
  }, HOLD_MS);
  const code = await new Promise((done) => child.on("close", done));
  clearTimeout(hold);
  const elapsed = Date.now() - started;
  console.log(
    name.padEnd(20),
    "exit",
    code,
    elapsed < RELEASED_MS ? "released stdin" : "waited for the pipe",
  );
}

// ref() and unref() are the socket methods, and they return the stream. Asked
// of a CHILD rather than this process: node only gives stdin a socket when it
// is a pipe or a TTY, so whether these exist at all depends on what the
// harness happened to attach -- a child's stdin is always a pipe here.
{
  const file = path.join(dir, "shape.mjs");
  writeFileSync(
    file,
    "process.stdout.write(JSON.stringify([typeof process.stdin.ref, typeof process.stdin.unref," +
      " process.stdin.unref() === process.stdin, process.stdin.ref() === process.stdin]));\n",
  );
  const child = spawn(process.execPath, [file], { stdio: ["pipe", "pipe", "inherit"] });
  let out = "";
  child.stdout.on("data", (chunk) => {
    out += chunk;
  });
  await new Promise((done) => child.on("close", done));
  console.log("ref/unref shape", out);
}

rmSync(dir, { recursive: true, force: true });
