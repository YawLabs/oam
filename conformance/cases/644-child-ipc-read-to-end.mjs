// The IPC channel is read to the child's end: a child that sends its result
// and exits at once still has every message delivered, on the spawn
// (stdio 'ipc') channel and on fork()'s alike, and the parent sees exactly one
// 'disconnect'. A second disconnect() does not emit a second 'disconnect'.
import { fork, spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-ipc-tail-${process.pid}`);
mkdirSync(dir, { recursive: true });

// Sends argv[2] messages, the last one large, and exits from the last send's
// callback: nothing keeps the child alive once its result is handed over.
const sendAndExit = path.join(dir, "send-and-exit.cjs");
writeFileSync(
  sendAndExit,
  "const n=Number(process.argv[2]);" +
    "for(let i=0;i<n-1;i++)process.send({i});" +
    "process.send({i:n-1,pad:'x'.repeat(200000)},()=>process.exit(0));",
);
// Stays up until the parent disconnects.
const waiter = path.join(dir, "waiter.cjs");
writeFileSync(waiter, "process.on('message',()=>{});process.on('disconnect',()=>process.exit(0));");

const settle = (cp) =>
  new Promise((done) => {
    const seen = { messages: 0, bytes: 0, disconnects: 0, exit: null };
    let exited = false;
    let disconnected = false;
    const finish = () => {
      // A beat past both, so a duplicate 'disconnect' would be counted.
      if (exited && disconnected) setTimeout(() => done(seen), 100);
    };
    cp.on("message", (m) => {
      seen.messages++;
      if (m.pad) seen.bytes += m.pad.length;
    });
    cp.on("disconnect", () => {
      seen.disconnects++;
      disconnected = true;
      finish();
    });
    cp.on("exit", (code) => {
      seen.exit = code;
      exited = true;
      finish();
    });
  });

try {
  const spawned = spawn(process.execPath, [sendAndExit, "5"], {
    stdio: ["ignore", "ignore", "ignore", "ipc"],
  });
  console.log("spawn ipc:", JSON.stringify(await settle(spawned)), "connected", spawned.connected);

  const forked = fork(sendAndExit, ["5"]);
  console.log("fork:", JSON.stringify(await settle(forked)), "connected", forked.connected);

  // disconnect() from the parent: one 'disconnect', and the child goes.
  const held = fork(waiter);
  const heldDone = settle(held);
  held.send({ go: 1 }, () => held.disconnect());
  console.log("parent disconnect:", JSON.stringify(await heldDone), "connected", held.connected);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
