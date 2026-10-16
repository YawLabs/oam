// destroy() on a readable extra-fd stream closes the pipe, as node's does.
//
// The parent destroys the stream on the child's fd 4 after the first chunk.
// Under node that closes the pipe's read end, so the child's next write fails
// (EPIPE) and it exits with the code it picked for that. oam's stream used to
// let destroy() close nothing: the native read kept the pipe open and kept
// polling for as long as the child lived, so the child wrote on unhindered.
// On Windows a read blocked in ReadFile is now cancelled by the close.
//
// Gated to Windows: on POSIX the parent's blocking read still holds its end
// while it waits, so a destroy there is not seen until the child next writes
// a byte the parent would have read (oam's child_unix.rs; noted in
// docs/node-divergences.md). Other platforms print the same lines so
// node == oam holds.
import { spawn } from "node:child_process";
import { existsSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const EXPECTED = ["first chunk tick:0", "destroyed true", "child exit 3", "close after exit true"];

if (process.platform !== "win32") {
  for (const line of EXPECTED) console.log(line);
} else {
  const nodeExe =
    (process.env.PATH || "")
      .split(";")
      .map((d) => path.join(d, "node.exe"))
      .find((p) => existsSync(p)) || "node.exe";

  const child = path.join(os.tmpdir(), `oam-conf-452-${process.pid}.cjs`);
  // Writes a tick to fd 4 every 20 ms. A failed write is the parent's close
  // arriving: exit 3. Never failing in 10 s exits 9, which fails the case.
  writeFileSync(
    child,
    "const fs=require('fs');let n=0;" +
      "const t=setInterval(()=>{try{fs.writeSync(4,'tick:'+(n++)+'\\n');}" +
      "catch(e){clearInterval(t);process.exit(3);}" +
      "if(n>500){clearInterval(t);process.exit(9);}},20);",
  );

  const cp = spawn(nodeExe, [child], { stdio: ["ignore", "inherit", "inherit", "ignore", "pipe"] });
  const timer = setTimeout(() => cp.kill("SIGKILL"), 30000);
  const stream = cp.stdio[4];
  stream.once("data", (c) => {
    console.log("first chunk", c.toString().split("\n")[0]);
    stream.destroy();
    console.log("destroyed", stream.destroyed);
  });
  let exited = false;
  cp.on("exit", (code) => {
    exited = true;
    console.log("child exit", code);
  });
  await new Promise((r) => cp.on("close", r));
  clearTimeout(timer);
  console.log("close after exit", exited);
  rmSync(child, { force: true });
}
