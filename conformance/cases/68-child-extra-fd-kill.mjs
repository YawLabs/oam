// Extra-fd kill/exit-shape parity for the Unix raw-child path (spawn with
// numbered fds beyond 0/1/2 routes through oam's spawn_extra/raw_kill/raw_wait,
// the CDP-over-pipe machinery). Pins the exit report against Node's real
// behavior, which the differential oracle supplies live:
//
//   1. kill() with a signal the child does NOT trap -> {code:null, signal}.
//   2. kill('SIGUSR1') against a child that TRAPS SIGUSR1 and exit(0)s ->
//      {code:0, signal:null}. The recorded kill must not masquerade as a
//      signal death; WIFSIGNALED is the source of truth. (Regression guard for
//      the raw_wait signal-precedence fix.)
//   3. kill() on an extra-fd child that exits on its own, issued after it is
//      already dead -> the real exit code survives, no phantom signal.
//
// Unix-only by nature (POSIX signals); on other platforms print the same
// expected lines so node==oam holds (mirrors case 23).
import { spawn } from "node:child_process";
import { writeFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported = process.platform === "linux" || process.platform === "darwin";

if (!supported) {
  console.log("sigterm code=null signal=SIGTERM");
  console.log("trapped code=0 signal=null");
  console.log("late-kill code=0 signal=null");
} else {
  const dir = os.tmpdir();
  const stdio = ["ignore", "pipe", "pipe", "pipe", "pipe"];

  // 1. Untrapped SIGTERM kills.
  const sleeper = path.join(dir, `oam-conf-kill-sleeper-${process.pid}.cjs`);
  writeFileSync(sleeper, "setTimeout(()=>process.exit(0), 30000);");
  const r1 = await new Promise((done) => {
    const cp = spawn(process.execPath, [sleeper], { stdio });
    cp.on("spawn", () => setTimeout(() => cp.kill("SIGTERM"), 150));
    cp.on("close", (code, signal) => done(`sigterm code=${code} signal=${signal}`));
    setTimeout(() => done("sigterm TIMEOUT"), 8000);
  });
  console.log(r1);
  rmSync(sleeper, { force: true });

  // 2. Trapped SIGUSR1 -> the child survives the signal and exits 0 on its own.
  const trapper = path.join(dir, `oam-conf-kill-trap-${process.pid}.cjs`);
  writeFileSync(
    trapper,
    "process.on('SIGUSR1', () => process.exit(0)); setTimeout(()=>process.exit(9), 30000);",
  );
  const r2 = await new Promise((done) => {
    const cp = spawn(process.execPath, [trapper], { stdio });
    cp.on("spawn", () => setTimeout(() => cp.kill("SIGUSR1"), 200));
    cp.on("close", (code, signal) => done(`trapped code=${code} signal=${signal}`));
    setTimeout(() => done("trapped TIMEOUT"), 8000);
  });
  console.log(r2);
  rmSync(trapper, { force: true });

  // 3. Signal delivered after the child already exited is a no-op; the real
  //    exit code is preserved.
  const quick = path.join(dir, `oam-conf-kill-quick-${process.pid}.cjs`);
  writeFileSync(quick, "process.exit(0);");
  const r3 = await new Promise((done) => {
    const cp = spawn(process.execPath, [quick], { stdio });
    cp.on("close", (code, signal) => {
      // The child is already reaped by the time close fires; a late kill must
      // not change the report.
      cp.kill("SIGTERM");
      done(`late-kill code=${code} signal=${signal}`);
    });
    setTimeout(() => done("late-kill TIMEOUT"), 8000);
  });
  console.log(r3);
  rmSync(quick, { force: true });
}
