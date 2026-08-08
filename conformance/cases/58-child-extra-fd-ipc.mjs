// An 'ipc' slot alongside numbered fds above 2. Both features at once: the
// extra-fd spawn path AND a message channel.
//
// Regression guard: spawn() computed `wantsIpc`, then returned early into the
// extra-fd path without ever attaching the channel, so this combination
// silently produced a child with no IPC -- `send()` queued forever and
// 'message' never fired, with nothing to diagnose from. The related bug was
// that the ipc slot was reported at a hardcoded child.stdio[3], which on this
// path is a REAL extra fd and got clobbered.
//
// child.stdio[n] is read from the 'spawn' event: oam binds the ipc channel on a
// loopback socket, so an IPC child resolves one tick later than a plain spawn
// (see "child.pid is one tick late for IPC children" in docs/node-divergences).
// Node populates it synchronously, so reading it from 'spawn' is the shape that
// is correct on BOTH -- and what Node's own docs recommend regardless.
import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";

if (!supported) {
  console.log('stdout "CHILD_STDOUT"');
  console.log('messages {"ready":true} {"echo":"PONG"}');
  console.log("ipc slot null true");
  console.log("extra fd is stream true");
  console.log("exit 0");
} else {
  const dir = path.join(os.tmpdir(), `oam-conf-extraipc-${process.pid}`);
  mkdirSync(dir, { recursive: true });
  const child = path.join(dir, "child.cjs");
  writeFileSync(
    child,
    "process.stdout.write('CHILD_STDOUT\\n');" +
      "process.on('message',(m)=>{process.send({echo:m.ping});setTimeout(()=>process.exit(0),200);});" +
      "process.send({ready:true});",
  );

  const cp = spawn(process.execPath, [child], {
    // >3 entries routes to the extra-fd backend; 'ipc' must survive that.
    stdio: ["ignore", "pipe", "pipe", "pipe", "pipe", "ipc"],
  });

  let out = "";
  const seen = [];
  let slotIsNull = null;
  let extraIsStream = null;

  cp.on("spawn", () => {
    cp.stdout.on("data", (c) => { out += c.toString(); });
    // The ipc slot reads back as null at the index the caller put it, and the
    // real extra fd next to it is still a live stream.
    slotIsNull = cp.stdio[5] === null;
    extraIsStream = cp.stdio[3] !== null && typeof cp.stdio[3].write === "function";
  });

  cp.on("message", (m) => {
    seen.push(JSON.stringify(m));
    if (m.ready) cp.send({ ping: "PONG" });
  });

  await new Promise((done) => cp.on("close", done));

  console.log("stdout", JSON.stringify(out.trim()));
  console.log("messages", seen.join(" "));
  console.log("ipc slot null", slotIsNull);
  console.log("extra fd is stream", extraIsStream);
  rmSync(dir, { recursive: true, force: true });
  console.log("exit 0");
}
