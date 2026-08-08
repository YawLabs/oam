// exec()'s maxBuffer binds BOTH streams, and the overflow is reported as
// ERR_CHILD_PROCESS_STDIO_MAXBUFFER rather than as the kill that implements it.
//
// Regression guard, two defects at once: maxBuffer used to be enforced on
// stdout only, so a child that only wrote to stderr grew the collector array
// without limit; and the stdout check measured length by re-running
// Buffer.concat over everything accumulated so far on EVERY chunk, which is
// quadratic (gigabytes of copying at the 50MB default). Both are observable
// here only through the error contract, so that is what this pins.
import { exec } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const dir = path.join(os.tmpdir(), `oam-conf-maxbuf-${process.pid}`);
mkdirSync(dir, { recursive: true });

// Writes `bytes` to the stream named in argv[3], well past the tiny maxBuffer
// the test sets, in chunks so the limit is crossed mid-stream rather than on
// the first write.
const noisy = path.join(dir, "noisy.cjs");
writeFileSync(
  noisy,
  "const which=process.argv[2];const total=Number(process.argv[3]);" +
    "const chunk='x'.repeat(1024);" +
    "for(let n=0;n<total;n+=chunk.length){" +
    "  if(which==='stderr')process.stderr.write(chunk);else process.stdout.write(chunk);" +
    "}",
);

const runExec = (which) =>
  new Promise((done) => {
    // Quote the exe: on Windows it lives under a path with spaces.
    const cmd = `"${process.execPath}" ${JSON.stringify(noisy)} ${which} 65536`;
    // 4000, NOT 4096: the child writes in 1024-byte chunks, so a limit that is
    // a multiple of the chunk size is reached exactly on a boundary and
    // "truncate the crossing chunk" and "drop the crossing chunk" produce the
    // SAME byte count. 4000 puts the limit mid-chunk, where they differ
    // (4000 vs 3072) and the assertion below can actually tell them apart.
    exec(cmd, { maxBuffer: 4000 }, (err, stdout, stderr) => {
      done({
        code: err && err.code,
        message: err && err.message,
        outLen: (stdout || "").length,
        errLen: (stderr || "").length,
      });
    });
  });

for (const which of ["stdout", "stderr"]) {
  const r = await runExec(which);
  console.log(`${which} code`, r.code);
  console.log(`${which} message`, JSON.stringify(r.message));
  // The EXACT byte count, not a `<= maxBuffer` bound. node delivers precisely
  // maxBuffer bytes on overflow: it truncates the chunk that crosses the limit
  // and keeps the prefix. An implementation that instead DROPS that whole chunk
  // also satisfies `<=`, while handing back a shorter, run-dependent prefix --
  // so the bound was satisfied by both behaviors and could not see the
  // difference. maxBuffer is deliberately not a multiple of the child's 1024-
  // byte write, so a dropped chunk cannot coincidentally land on the limit.
  console.log(`${which} delivered`, which === "stdout" ? r.outLen : r.errLen);
}

// Under the limit the command still succeeds and delivers its output intact.
const ok = await new Promise((done) => {
  const cmd = `"${process.execPath}" ${JSON.stringify(noisy)} stdout 1024`;
  exec(cmd, { maxBuffer: 1024 * 1024 }, (err, stdout) => {
    done({ err: err && err.code, len: (stdout || "").length });
  });
});
console.log("under limit", ok.err, ok.len);

rmSync(dir, { recursive: true, force: true });
console.log("exit 0");
