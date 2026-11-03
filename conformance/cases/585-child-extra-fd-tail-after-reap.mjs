// What an extra-fd child writes just before it exits reaches the parent, even
// when the child is reaped while no read of that pipe is in flight (#456).
//
// The extra-fd backend (a stdio array past fd 2) closed every parent-side
// pipe end when it reaped the child, read ends included, so whatever was
// still in the pipe was dropped and the stream ended early -- conformance
// case 63 lost its second stdout line that way once under load on Linux. The
// Windows backend had the same bug and was fixed by #419; this pins both.
//
// Leg 1 forces the losing order without depending on load: the parent blocks
// its JS thread right after the first chunk arrives, so no next read can be
// issued while the child writes its tail and exits. Leg 2 is case 63's shape
// (write, wait for stdin, write again, exit) run as parallel copies.
//
// Spawns `node` from PATH, as case 63 does: an oam child cannot yet receive
// inherited numbered fds above 2 (docs/node-divergences.md #19), and the
// stdio array has to reach fd 4 to select the extra-fd backend. Gated like 63:
// that backend exists on win32/linux/darwin only, and elsewhere the expected
// lines are printed so node == oam still holds.
import { spawn } from "node:child_process";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";
const COPIES = 16;

if (!supported) {
  console.log('blocked reader got "A|B" exit 0');
  console.log(`parallel copies with the whole stdout ${COPIES}/${COPIES}`);
} else {
  const STDIO = ["pipe", "pipe", "ignore", "ignore", "pipe"];

  function collect(child) {
    const chunks = [];
    child.stdout.on("data", (c) => chunks.push(c));
    return new Promise((resolve) => {
      child.on("close", (code) => resolve({ out: Buffer.concat(chunks).toString(), code }));
    });
  }

  // Leg 1: "A", a pause, "B", exit; the parent's JS thread is busy for well
  // past the child's exit from the moment "A" arrives.
  {
    const child = spawn(
      "node",
      ["-e", "const fs=require('fs');fs.writeSync(1,'A|');setTimeout(()=>{fs.writeSync(1,'B');process.exit(0)},300)"],
      { stdio: STDIO },
    );
    child.stdin.end();
    let blocked = false;
    child.stdout.once("data", () => {
      if (blocked) return;
      blocked = true;
      const until = Date.now() + 1500;
      while (Date.now() < until) {
        // The pipe's next read cannot be issued from here.
      }
    });
    const { out, code } = await collect(child);
    console.log(`blocked reader got ${JSON.stringify(out)} exit ${code}`);
  }

  // Leg 2: case 63's child A, stdout and stdin only.
  {
    const script =
      "const fs=require('fs');" +
      "fs.writeSync(1,'OUT_FROM_CHILD\\n');" +
      "const c=Buffer.alloc(32);const m=fs.readSync(0,c,0,32,null);" +
      "fs.writeSync(1,'STDIN_GOT:'+c.slice(0,m).toString().trim()+'\\n');" +
      "process.exit(0);";
    const runs = Array.from({ length: COPIES }, () => {
      const child = spawn("node", ["-e", script], { stdio: STDIO });
      const done = collect(child);
      child.stdin.end("SIN");
      return done;
    });
    const results = await Promise.all(runs);
    const whole = results.filter(
      ({ out, code }) => code === 0 && out.split(/\r?\n/).filter(Boolean).join(" ") === "OUT_FROM_CHILD STDIN_GOT:SIN",
    ).length;
    console.log(`parallel copies with the whole stdout ${whole}/${COPIES}`);
  }
}
