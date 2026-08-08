// What a child that NEVER STARTS owes its caller, and what execFile owes its
// arguments. Four regressions, all of which passed every other test in this
// suite because they live on paths the happy-path cases never take.
//
//   1. Writing to a failed child's stdin KILLED THE PROCESS. The spawn error was
//      handed to the stdin Writable's write callback, which errors the stream;
//      nobody listens on child.stdin (callers listen on the CHILD), so it was an
//      uncaught exception. `cp.on('error', h); cp.stdin.end(payload)` -- the most
//      ordinary "pipe input into a tool that may not be installed" shape -- died
//      with exit 1 even though the caller handled the failure correctly.
//   2. A failed spawn emitted no 'close'. Node follows 'error' with 'close'
//      carrying the libuv errno, so every wrapper whose completion path is
//      'close' stalled forever instead of taking its error branch.
//   3. A write issued from INSIDE the child's 'error' handler never settled:
//      'spawnfail' is one-shot and had already fired, so the callback dangled.
//   4. execFile() joined argv into one string and ran it through a shell, so
//      arguments were re-split on whitespace (and shell metacharacters inside an
//      argument were executed). Node's execFile is shell-free by design.
//
// Every observable is printed by THIS process; the children here either do not
// exist or write only to a captured pipe, so ordering is deterministic.
import { execFile, spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const MISSING = "oam-no-such-binary-zzz";
const dir = path.join(os.tmpdir(), `oam-conf-failpath-${process.pid}`);
mkdirSync(dir, { recursive: true });

// 1 + 2. Handle 'error' the way a caller is supposed to, then write to stdin.
//        Surviving this block at all is assertion #1; 'close' is #2.
const first = await new Promise((done) => {
  const cp = spawn(MISSING, []);
  const seen = { code: null, close: "NEVER", writeCb: "NEVER" };
  cp.on("error", (e) => { seen.code = e.code; });
  cp.on("close", (code, signal) => {
    seen.close = `${code} ${signal}`;
    // 'close' is the completion signal, so resolving here is itself the proof
    // that it fired -- the timeout below only exists to fail loudly if it does
    // not, rather than hanging the whole suite.
    done(seen);
  });
  cp.stdin.write("payload", (e) => { seen.writeCb = e ? e.code : "none"; });
  cp.stdin.end();
  setTimeout(() => done(seen), 4000);
});
console.log("survived stdin write true");
console.log("error code", first.code);
console.log("close", first.close);
console.log("pending write cb", first.writeCb);

// 3. A write registered AFTER the one-shot failure has already fired.
const late = await new Promise((done) => {
  const cp = spawn(MISSING, []);
  cp.on("error", () => {
    cp.stdin.write("late", (e) => done(e ? e.code : "none"));
    cp.stdin.end();
  });
  setTimeout(() => done("DANGLED"), 4000);
});
console.log("late write cb", late);

// 4. execFile must not use a shell: a path AND an argument that both contain a
//    space survive intact, which shell re-splitting cannot do.
const script = path.join(dir, "a b.cjs");
writeFileSync(script, "console.log('ARGV2=' + JSON.stringify(process.argv[2]));");
const ef = await new Promise((done) => {
  execFile(process.execPath, [script, "two words"], (err, stdout) => {
    done({ err: err ? (err.code ?? "err") : "none", out: (stdout || "").trim() });
  });
});
console.log("execFile err", ef.err);
console.log("execFile argv", ef.out);

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
