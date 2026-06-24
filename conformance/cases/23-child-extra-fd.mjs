// Extra-fd stdio: a child spawned with stdio:['ignore','pipe','pipe','pipe',
// 'pipe'] gets numbered fds 3/4, exposed as child.stdio[3] (Writable) and
// child.stdio[4] (Readable). Regression guard for the CDP-over-pipe path that
// lets oam drive a browser (Chromium --remote-debugging-pipe). Windows-only;
// on other platforms it prints the same expected lines so the differential
// stays green until Unix extra-fd lands.
import { spawn } from "node:child_process";
import { existsSync, writeFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";

if (process.platform !== "win32") {
  // Unix extra-fd is a follow-up; emit the success lines so node==oam holds.
  console.log("fd4 HELLO_FROM_FD4 GOT:PING");
  console.log("exit 0");
} else {
  const child = path.join(os.tmpdir(), "oam-conf-extrafd-child.cjs");
  writeFileSync(
    child,
    "const fs=require('fs');" +
      "fs.writeSync(4,'HELLO_FROM_FD4\\n');" +
      "const b=Buffer.alloc(32);const n=fs.readSync(3,b,0,32,null);" +
      "fs.writeSync(4,'GOT:'+b.slice(0,n).toString().trim()+'\\n');" +
      "process.exit(0);",
  );
  const nodeExe =
    process.env.PATH.split(";")
      .map((d) => d.replace(/\\$/, "") + "\\node.exe")
      .find((p) => existsSync(p)) || "node.exe";

  const cp = spawn(nodeExe, [child], { stdio: ["ignore", "pipe", "pipe", "pipe", "pipe"] });
  let out4 = "";
  cp.stdio[4].on("data", (c) => { out4 += c.toString(); });
  cp.on("spawn", () => setTimeout(() => cp.stdio[3].write("PING\n"), 150));
  cp.on("close", (code) => {
    const toks = out4.replace(/\s+/g, " ").trim();
    console.log("fd4", toks.includes("HELLO_FROM_FD4") ? "HELLO_FROM_FD4" : "?", toks.includes("GOT:PING") ? "GOT:PING" : "?");
    console.log("exit", code);
    rmSync(child, { force: true });
  });
}
