// Null/undefined slots in an extra-fd stdio array default BY FD, which is
// node's documented rule: 'pipe' for fds 0/1/2, 'ignore' for fd 3 and above.
//
// Regression guard: the extra-fd backend mapped null/undefined to 'ignore' at
// EVERY index, so stdio:[null,null,null,'pipe','pipe'] produced a child with
// child.stdin/stdout/stderr all null instead of three live pipes. Nothing threw
// -- the caller just never saw the child's output, and writing to the missing
// stdin was a TypeError far from the spawn. The 'ignore' default ABOVE fd 2 is
// the other half of the same rule and must not regress into a pipe.
//
// Spawns `node` resolved from PATH as the child rather than process.execPath:
// oam can SPAWN extra-fd children but cannot yet RECEIVE inherited numbered fds
// above 2 (an oam child's fs.writeSync(4,...) throws EBADF -- divergence 19 in
// docs/node-divergences.md), and the fd3/fd4 half of this case needs a child
// that can. Platform-gated exactly like 23-child-extra-fd: the extra-fd backend
// exists on win32/linux/darwin only, and elsewhere the same expected lines are
// printed so node==oam still holds.
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";

if (!supported) {
  console.log("A slots are streams true true true");
  console.log('A stdout "OUT_FROM_CHILD STDIN_GOT:SIN"');
  console.log('A stderr "ERR_FROM_CHILD"');
  console.log("A fd4 FD4_HELLO FD3_GOT:PING");
  console.log("A exit 0");
  console.log("B slot3 is stream true");
  console.log("B slot5 null true");
  console.log("B slot6 null true");
  console.log("B explicit-ignore stdin null true");
  console.log('B stdout "B_OUT"');
  console.log("B exit 0");
} else {
  const dir = path.join(os.tmpdir(), `oam-conf-nullslot-${process.pid}`);
  mkdirSync(dir, { recursive: true });

  // fs.writeSync on the numbered fd, not process.stdout.write: a pipe-backed
  // stdout is async, and this child exits the moment it is done.
  const childA = path.join(dir, "a.cjs");
  writeFileSync(
    childA,
    "const fs=require('fs');" +
      "fs.writeSync(1,'OUT_FROM_CHILD\\n');" +
      "fs.writeSync(2,'ERR_FROM_CHILD\\n');" +
      "fs.writeSync(4,'FD4_HELLO\\n');" +
      "const b=Buffer.alloc(32);const n=fs.readSync(3,b,0,32,null);" +
      "fs.writeSync(4,'FD3_GOT:'+b.slice(0,n).toString().trim()+'\\n');" +
      "const c=Buffer.alloc(32);const m=fs.readSync(0,c,0,32,null);" +
      "fs.writeSync(1,'STDIN_GOT:'+c.slice(0,m).toString().trim()+'\\n');" +
      "process.exit(0);",
  );

  const childB = path.join(dir, "b.cjs");
  writeFileSync(childB, "require('fs').writeSync(1,'B_OUT\\n');process.exit(0);");

  const isWin = process.platform === "win32";
  const exe = isWin ? "node.exe" : "node";
  const sep = isWin ? ";" : ":";
  const nodeExe =
    (process.env.PATH || "")
      .split(sep)
      .map((d) => path.join(d, exe))
      .find((p) => existsSync(p)) || exe;

  const norm = (s) => JSON.stringify(s.replace(/\s+/g, " ").trim());

  // ---- A: null/undefined in slots 0/1/2 must become real pipes -------------
  // Mixed null and undefined: both are "use the default for this fd".
  const a = spawn(nodeExe, [childA], {
    stdio: [null, undefined, null, "pipe", "pipe"],
  });

  let aOut = "";
  let aErr = "";
  let aFd4 = "";
  let aSlots = "";

  a.on("spawn", () => {
    const isStream = (s) => s !== null && s !== undefined && typeof s.on === "function";
    aSlots = `${isStream(a.stdin)} ${isStream(a.stdout)} ${isStream(a.stderr)}`;
    // Optional-chained so a regression reports as `false` + empty capture
    // rather than a TypeError with a runtime-specific stack.
    a.stdout?.on("data", (c) => { aOut += c.toString(); });
    a.stderr?.on("data", (c) => { aErr += c.toString(); });
    a.stdio[4]?.on("data", (c) => { aFd4 += c.toString(); });
    setTimeout(() => {
      a.stdio[3]?.write("PING\n");
      a.stdin?.write("SIN\n");
      a.stdin?.end();
    }, 150);
  });

  const aCode = await new Promise((done) => a.on("close", done));

  console.log("A slots are streams", aSlots);
  console.log("A stdout", norm(aOut));
  console.log("A stderr", norm(aErr));
  const t = aFd4.replace(/\s+/g, " ").trim();
  console.log(
    "A fd4",
    t.includes("FD4_HELLO") ? "FD4_HELLO" : "?",
    t.includes("FD3_GOT:PING") ? "FD3_GOT:PING" : "?",
  );
  console.log("A exit", aCode);

  // ---- B: null/undefined at fd >= 3 must stay 'ignore' --------------------
  // Real extra fds at 3/4 alongside defaulted-away slots at 5/6, and an
  // EXPLICIT 'ignore' at slot 0 for contrast with A's defaulted pipe.
  const b = spawn(nodeExe, [childB], {
    stdio: ["ignore", "pipe", "pipe", "pipe", "pipe", null, undefined],
  });

  let bOut = "";
  let bSlot3 = null;
  let bSlot5 = null;
  let bSlot6 = null;
  let bStdin = null;

  b.on("spawn", () => {
    bSlot3 = b.stdio[3] !== null && typeof b.stdio[3].write === "function";
    bSlot5 = b.stdio[5] === null;
    bSlot6 = b.stdio[6] === null;
    bStdin = b.stdin === null;
    b.stdout?.on("data", (c) => { bOut += c.toString(); });
  });

  const bCode = await new Promise((done) => b.on("close", done));

  console.log("B slot3 is stream", bSlot3);
  console.log("B slot5 null", bSlot5);
  console.log("B slot6 null", bSlot6);
  console.log("B explicit-ignore stdin null", bStdin);
  console.log("B stdout", norm(bOut));
  console.log("B exit", bCode);

  rmSync(dir, { recursive: true, force: true });
}
