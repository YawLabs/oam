// Seven pre-existing child_process defects, all surfaced by an adversarial
// review of the stdio work rather than by any failing test -- they sit on paths
// (option reuse, pre-handle kill, timeouts, error shape) that nothing exercised.
//
//   1. spawn() spliced 'ipc' out of the CALLER's options object, so reusing one
//      options literal for a worker pool gave child 0 a channel and every later
//      child none -- cp.send undefined, with no error anywhere.
//   2. kill() was a silent no-op while the native handle was still resolving:
//      it returned true, `killed` stayed false, and the child ran on.
//   3. exec() accepted `timeout` and ignored it, so the standard way to bound a
//      shell-out did not bound it.
//   4. execSync()'s thrown error had no `.output`, the 3-slot array harnesses
//      read to get both streams from one throw -- and its message appended a
//      newline node only appends when there IS stderr.
//   5. fork() accepted an explicit stdio array with no 'ipc' entry; node throws
//      ERR_CHILD_PROCESS_IPC_REQUIRED, and losing that guard lets code be
//      written here that is fatal on node.
//   6. spawnSync()'s `timeout` returned ETIMEDOUT without killing the child.
//   7. spawnSync() truncated at maxBuffer and reported success, so a caller got
//      a short result that looked complete instead of node's ENOBUFS.
import { execSync, fork, spawn, spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-followups-${process.pid}`);
mkdirSync(dir, { recursive: true });

const quiet = path.join(dir, "quiet.cjs");
writeFileSync(quiet, "process.exit(0);");
const failing = path.join(dir, "failing.cjs");
writeFileSync(failing, "process.exit(3);");
const noisy = path.join(dir, "noisy.cjs");
writeFileSync(noisy, "const c='x'.repeat(1024);for(let i=0;i<64;i++)process.stdout.write(c);");
const sleeper = path.join(dir, "sleeper.cjs");
writeFileSync(sleeper, "setTimeout(()=>process.exit(0), 30000);");
const forkable = path.join(dir, "forkable.cjs");
writeFileSync(forkable, "process.send&&process.send({ok:1});setTimeout(()=>process.exit(0),150);");

// 1. One options object, two children. The second must still get a channel.
const shared = { stdio: ["pipe", "pipe", "pipe", "ipc"] };
const a = spawn(process.execPath, [forkable], shared);
const b = spawn(process.execPath, [forkable], shared);
console.log("reuse: send on both", typeof a.send === "function", typeof b.send === "function");
console.log("reuse: options untouched", JSON.stringify(shared.stdio));
await Promise.all([a, b].map((cp) => new Promise((r) => { cp.on("close", r); cp.on("error", r); })));

// 2. kill() the instant spawn() returns -- before any handle can have resolved
//    on the ipc path. The child must actually die.
const killed = await new Promise((done) => {
  const cp = spawn(process.execPath, [sleeper], { stdio: ["pipe", "pipe", "pipe", "ipc"] });
  cp.kill();
  cp.on("close", (code, signal) => done(`closed signal=${signal !== null}`));
  cp.on("error", () => done("error"));
  setTimeout(() => done("STILL RUNNING"), 8000);
});
console.log("pre-handle kill:", killed);

// 3. exec() must honor `timeout` and report the child as killed.
const { exec } = await import("node:child_process");
const timedOut = await new Promise((done) => {
  exec(`"${process.execPath}" ${JSON.stringify(sleeper)}`, { timeout: 700 }, (err) => {
    done(err ? `${err.killed} ${err.signal}` : "NO ERROR");
  });
});
console.log("exec timeout:", timedOut);

// 4. execSync's error carries the whole spawnSync result, and the message has no
//    trailing newline when stderr is empty.
let sync = "NO THROW";
try {
  execSync(`"${process.execPath}" ${JSON.stringify(failing)}`);
} catch (err) {
  sync = [
    `status=${err.status}`,
    `output=${Array.isArray(err.output) ? err.output.length : "MISSING"}`,
    `output0null=${Array.isArray(err.output) ? err.output[0] === null : "n/a"}`,
    `endsWithNewline=${/\n$/.test(err.message)}`,
  ].join(" ");
}
console.log("execSync error:", sync);

// 5. fork with an explicit stdio array and no channel in it.
let forkThrow = "NO THROW";
try {
  fork(forkable, [], { stdio: ["pipe", "pipe", "pipe"] });
} catch (err) {
  forkThrow = err.code;
}
console.log("fork without ipc:", forkThrow);

// 6. spawnSync timeout: the result says ETIMEDOUT, and -- the part that was
//    missing -- the child is dead by the time it returns.
const st = spawnSync(process.execPath, [sleeper], { timeout: 700 });
console.log("spawnSync timeout:", st.error ? st.error.code : "none", "signal:", st.signal);

// 7. spawnSync maxBuffer is an ERROR, not a quiet trim. The retained byte count
//    is read-granularity dependent on both runtimes, so only the shape is
//    pinned -- length deliberately excluded.
const ob = spawnSync(process.execPath, [noisy], { maxBuffer: 4000 });
console.log(
  "spawnSync maxBuffer:",
  ob.error ? ob.error.code : "none",
  "status:", ob.status,
  "signal:", ob.signal,
  "kept:", ob.stdout !== null && ob.stdout.length > 0,
);

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
