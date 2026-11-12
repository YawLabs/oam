// Extra-fd read EOF vs error discrimination on the raw-child path. The parent
// reads a numbered fd whose child's write end closes; raw_read polls with no
// timeout and must turn the HUP into a clean EOF (stream 'end'), never an
// error, a truncated stream, or a hang. Two shapes:
//
//   1. Child writes a short frame to fd 4 then exits 0 -> parent sees the full
//      frame, then 'end', then a clean close code. Reading through EOF must not
//      surface an 'error' event.
//   2. Child writes nothing and dies immediately -> the parent read that was
//      pending on fd 4 resolves to 'end' (EOF), not a hang and not an error.
//      (Child death guarantees poll readability; the read returns 0.)
//
// Unix-only by nature (the raw-read poll path); other platforms print the same
// expected lines so node==oam holds (mirrors case 23).
import { spawn } from "node:child_process";
import { writeFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported = process.platform === "linux" || process.platform === "darwin";

if (!supported) {
  console.log("frame data=HELLO end=true error=false exit=0");
  console.log("death end=true error=false exit=7");
} else {
  const dir = os.tmpdir();
  const stdio = ["ignore", "pipe", "pipe", "pipe", "pipe"];

  function readToEnd(stream) {
    return new Promise((resolve) => {
      let data = "";
      let errored = false;
      stream.on("data", (c) => { data += c.toString(); });
      stream.on("error", () => { errored = true; });
      stream.on("end", () => resolve({ data: data.trim(), errored }));
    });
  }

  // 1. Short frame then clean exit.
  const writer = path.join(dir, `oam-conf-eof-writer-${process.pid}.cjs`);
  writeFileSync(writer, "require('fs').writeSync(4,'HELLO\\n'); process.exit(0);");
  const r1 = await new Promise((done) => {
    const cp = spawn(process.execPath, [writer], { stdio });
    const rd = readToEnd(cp.stdio[4]);
    cp.on("close", (code) => {
      rd.then(({ data, errored }) =>
        done(`frame data=${data} end=true error=${errored} exit=${code}`),
      );
    });
    setTimeout(() => done("frame TIMEOUT"), 8000);
  });
  console.log(r1);
  rmSync(writer, { force: true });

  // 2. Child dies with nothing written; the pending read must EOF, not hang.
  const dies = path.join(dir, `oam-conf-eof-dies-${process.pid}.cjs`);
  writeFileSync(dies, "process.exit(7);");
  const r2 = await new Promise((done) => {
    const cp = spawn(process.execPath, [dies], { stdio });
    const rd = readToEnd(cp.stdio[4]);
    cp.on("close", (code) => {
      rd.then(({ errored }) => done(`death end=true error=${errored} exit=${code}`));
    });
    setTimeout(() => done("death TIMEOUT"), 8000);
  });
  console.log(r2);
  rmSync(dies, { force: true });
}
