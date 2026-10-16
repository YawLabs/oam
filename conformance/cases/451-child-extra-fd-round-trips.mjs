// Many small messages each way over extra-fd stdio (more than three slots,
// the shape a CDP-over-pipe browser driver uses), then a burst a child writes
// just before it exits.
//
// On Windows the parent's read of such a pipe used to poll PeekNamedPipe and
// sleep 50 ms whenever it was empty, so every reply waited up to 50 ms to be
// seen: 20 round trips took about 1 s where node takes a few ms. A read is now
// one blocking ReadFile that returns the moment the child writes. The timing
// is not asserted (a loaded box makes any bound flaky); what is asserted is
// that every round trip comes back, in order, byte for byte.
//
// The burst covers the other half of that change: every byte a child writes
// just before it exits is read, as node reads a child's stdio to its EOF
// before 'close'. The read side used to be torn down the moment the child's
// exit was collected, so a read issued after that reported EOF; this burst
// passed anyway, because the read in flight at exit took the tail, and the
// deterministic check is oam_core's raw_read_after_wait_still_drains_the_pipe_win.
//
// The child is plain `node` on purpose (as in cases 23 and 92): the subject is
// the PARENT's pipe handling. Platforms with no extra-fd backend print the
// same lines so node == oam holds.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";
const ROUNDS = 50;
const BURST = 300 * 1024;
const burstBytes = Buffer.alloc(BURST, "abcdefghijklmnopqrstuvwxyz0123456789\n");
const burstHash = createHash("sha256").update(burstBytes).digest("hex").slice(0, 16);

const EXPECTED = [
  `round trips ${ROUNDS} in order`,
  `burst ${BURST} bytes sha256 ${burstHash}`,
  "close 0",
];

if (!supported) {
  for (const line of EXPECTED) console.log(line);
} else {
  const isWin = process.platform === "win32";
  const exe = isWin ? "node.exe" : "node";
  const sep = isWin ? ";" : ":";
  const nodeExe =
    (process.env.PATH || "")
      .split(sep)
      .map((d) => path.join(d, exe))
      .find((p) => existsSync(p)) || exe;

  const child = path.join(os.tmpdir(), `oam-conf-451-${process.pid}.cjs`);
  // Echo each line read on fd 3 back on fd 4 with a prefix. On "BURST" write
  // the burst to fd 4 and exit at once, with the tail still unread.
  writeFileSync(
    child,
    "const fs=require('fs');const b=Buffer.alloc(65536);let pending='';" +
      "for(;;){const n=fs.readSync(3,b,0,b.length,null);if(n===0)process.exit(2);" +
      "pending+=b.toString('latin1',0,n);let i;" +
      "while((i=pending.indexOf('\\n'))!==-1){const line=pending.slice(0,i);pending=pending.slice(i+1);" +
      "if(line==='BURST'){fs.writeSync(4,Buffer.alloc(" +
      BURST +
      ",'abcdefghijklmnopqrstuvwxyz0123456789\\n'));process.exit(0);}" +
      "fs.writeSync(4,'echo:'+line+'\\n');}}",
  );

  const cp = spawn(nodeExe, [child], { stdio: ["ignore", "inherit", "inherit", "pipe", "pipe"] });
  const timer = setTimeout(() => cp.kill("SIGKILL"), 30000);

  const replies = [];
  let buf = "";
  let burst = null;
  let sent = 0;
  const send = () => cp.stdio[3].write(`m${sent++}\n`);
  const chunks = [];
  const readDone = new Promise((resolve) => cp.stdio[4].on("end", resolve));
  cp.stdio[4].on("data", (c) => {
    if (burst) {
      chunks.push(c);
      return;
    }
    buf += c.toString("latin1");
    let i;
    while (!burst && (i = buf.indexOf("\n")) !== -1) {
      replies.push(buf.slice(0, i));
      buf = buf.slice(i + 1);
      if (replies.length < ROUNDS) send();
      else {
        burst = true;
        if (buf.length) chunks.push(Buffer.from(buf, "latin1"));
        cp.stdio[3].write("BURST\n");
      }
    }
  });
  send();

  const code = await new Promise((r) => cp.on("close", r));
  await readDone;
  clearTimeout(timer);

  const inOrder = replies.length === ROUNDS && replies.every((r, i) => r === `echo:m${i}`);
  console.log(`round trips ${replies.length} ${inOrder ? "in order" : JSON.stringify(replies.slice(0, 5))}`);
  const got = Buffer.concat(chunks);
  console.log(`burst ${got.length} bytes sha256 ${createHash("sha256").update(got).digest("hex").slice(0, 16)}`);
  console.log("close", code);
  rmSync(child, { force: true });
}
