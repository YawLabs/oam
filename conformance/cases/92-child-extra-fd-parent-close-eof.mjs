// The PARENT closes its held write end of a numbered fd, and the CHILD must
// see EOF on the matching read end.
//
// Case 67 asserts the other direction (the child closes fd 4, the parent's
// Readable emits 'end') and case 69-eof asserts the parent reading through a
// dead child's write end. Nothing asserted this one: `child.stdio[3].end()`
// runs the Writable's `final`, which is the only caller of the native
// close-fd op, and the fd it drops is the parent's copy of the pipe's write
// end. If ANY other live copy of that write end survives -- a stray dup, a
// non-CLOEXEC end leaked into the child at exec, an fd the registry never
// took ownership of -- the child's read never returns 0 and the pipe never
// EOFs.
//
// The failure mode is a HANG, not an error, which is why it needs a case with
// a hard kill: a missed EOF leaves the child blocked in readSync forever, so
// the timeout below SIGKILLs it rather than merely resolving around it.
//
// The child is plain `node` on purpose (as in case 23): the subject here is
// the PARENT's fd bookkeeping, so the child side stays a fixed reference
// implementation. Platforms with no extra-fd backend print the same lines so
// node == oam holds.
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";

const EXPECTED = ["fd3 read GOT:PING EOF:true", "exit 0"];

if (!supported) {
  for (const line of EXPECTED) console.log(line);
} else {
  const dir = path.join(os.tmpdir(), `oam-conf-closeeof-${process.pid}`);
  mkdirSync(dir, { recursive: true });
  const child = path.join(dir, "child.cjs");
  writeFileSync(
    child,
    "const fs=require('fs');" +
      "const b=Buffer.alloc(64);let got='';" +
      // Drain fd 3 until readSync reports 0. That zero IS the assertion: it can
      // only happen once every write end of the pipe is closed, and the parent
      // holds the only one.
      "for(;;){const n=fs.readSync(3,b,0,64,null);if(n===0)break;got+=b.slice(0,n).toString();}" +
      "fs.writeSync(4,'GOT:'+got.trim()+' EOF:true\\n');" +
      "process.exit(0);",
  );

  const isWin = process.platform === "win32";
  const exe = isWin ? "node.exe" : "node";
  const sep = isWin ? ";" : ":";
  const nodeExe =
    (process.env.PATH || "")
      .split(sep)
      .map((d) => path.join(d, exe))
      .find((p) => existsSync(p)) || exe;

  const cp = spawn(nodeExe, [child], { stdio: ["ignore", "pipe", "pipe", "pipe", "pipe"] });

  let out4 = "";
  cp.stdio[4].on("data", (c) => {
    out4 += c.toString();
  });
  cp.on("spawn", () =>
    setTimeout(() => {
      cp.stdio[3].write("PING\n");
      // The close under test. Everything after this line is the child proving
      // it saw the EOF.
      cp.stdio[3].end();
    }, 150),
  );

  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    cp.kill("SIGKILL");
  }, 10000);
  const code = await new Promise((done) => cp.on("close", done));
  clearTimeout(timer);

  const toks = out4.replace(/\s+/g, " ").trim();
  const verdict = timedOut
    ? "TIMEOUT (no EOF)"
    : toks.includes("GOT:PING EOF:true")
      ? "GOT:PING EOF:true"
      : `? ${JSON.stringify(toks)}`;
  console.log("fd3 read", verdict);
  console.log("exit", code);
  rmSync(dir, { recursive: true, force: true });
}
