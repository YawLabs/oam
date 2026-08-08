// oam as the CHILD of an extra-fd spawn -- the receive half of the CDP pipe
// transport, and previously divergence 19.
//
// oam could always SPAWN a child with numbered fds above 2, which is how it
// drives Chromium. It could not BE one: its descriptors are registry keys, not
// OS fds, and nothing had ever put an inherited descriptor in the registry, so
// `fs.writeSync(4, ...)` in an oam child threw EBADF. Case 23 works around that
// by spawning `node` as its child on purpose; THIS case spawns
// `process.execPath`, so under oam the child is oam and the inherited path is
// what is actually under test.
//
// Three things had to be true at once, and each is asserted below:
//   - an inherited fd is readable/writable at its real number
//   - oam's OWN descriptors cannot collide with it (the id counter used to
//     start at 3, exactly where a parent's fd 3 lands)
//   - closing an inherited fd closes the PARENT's descriptor, not just oam's
//     dup of it -- otherwise the peer never sees EOF, which for a CDP pipe is
//     the difference between "done" and a hang
import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";

const EXPECTED = [
  "fd4 FD4_HELLO",
  "fd3 GOT:PING",
  "own fd clear of inherited true",
  "fd4 saw EOF true",
  "exit 0",
];

if (!supported) {
  // Same lines on a platform with no extra-fd backend, so node == oam holds.
  for (const line of EXPECTED) console.log(line);
} else {
  const dir = path.join(os.tmpdir(), `oam-conf-selfhost-${process.pid}`);
  mkdirSync(dir, { recursive: true });
  const child = path.join(dir, "child.cjs");
  writeFileSync(
    child,
    "const fs=require('fs');" +
      "fs.writeSync(4,'FD4_HELLO\\n');" +
      "const b=Buffer.alloc(32);const n=fs.readSync(3,b,0,32,null);" +
      "fs.writeSync(4,'GOT:'+b.slice(0,n).toString().trim()+'\\n');" +
      // An own descriptor must land clear of the inherited ones. Opened AFTER
      // the inherited fds are in play, which is when a colliding allocator
      // would hand back 3 or 4 and shadow them.
      "const p=require('path').join(require('os').tmpdir(),'own-'+process.pid+'.txt');" +
      "const own=fs.openSync(p,'w');" +
      "fs.writeSync(4,'OWN_CLEAR:'+(own>4)+'\\n');" +
      "fs.closeSync(own);fs.unlinkSync(p);" +
      // Closing 4 must reach the real descriptor: the parent's 'end' below is
      // the assertion.
      "fs.closeSync(4);",
  );

  const cp = spawn(process.execPath, [child], {
    stdio: ["ignore", "pipe", "pipe", "pipe", "pipe"],
  });

  let out4 = "";
  let sawEof = false;
  cp.stdio[4].on("data", (c) => { out4 += c.toString(); });
  cp.stdio[4].on("end", () => { sawEof = true; });
  cp.on("spawn", () => setTimeout(() => cp.stdio[3].write("PING\n"), 200));

  const code = await new Promise((done) => cp.on("close", done));

  const lines = out4.trim().split("\n").map((l) => l.trim());
  const has = (needle) => lines.some((l) => l.includes(needle));
  console.log("fd4", has("FD4_HELLO") ? "FD4_HELLO" : `? ${JSON.stringify(lines)}`);
  console.log("fd3", has("GOT:PING") ? "GOT:PING" : `? ${JSON.stringify(lines)}`);
  console.log("own fd clear of inherited", has("OWN_CLEAR:true"));
  console.log("fd4 saw EOF", sawEof);
  rmSync(dir, { recursive: true, force: true });
  console.log("exit", code);
}
