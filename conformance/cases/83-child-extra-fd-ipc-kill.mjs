// The pending-kill flush at the spawnExtra + 'ipc' junction.
//
// An extra-fd spawn that also carries an 'ipc' slot defers its exec until the
// loopback channel is bound, so -- uniquely on the extra path -- kill() can
// arrive while the native handle is still null. Case 65 pins that window for
// fork(); nothing pinned it where the deferred launch is spawnExtra's, and
// the flush there is a separate code path (attachIpcChannel wrapping the
// spawnExtra launch, not the fork one). A dropped flush leaves the child
// alive for its full 30s timeout, so the 8s guard below converts that into a
// deterministic TIMEOUT line.
//
// Kill-shape case, so unix-only like case 68 (POSIX signal semantics); other
// platforms print the same expected line so node==oam holds.
import { spawn } from "node:child_process";
import { writeFileSync, rmSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const supported = process.platform === "linux" || process.platform === "darwin";

if (!supported) {
  console.log("ipc-kill code=null signal=SIGTERM");
} else {
  const sleeper = path.join(os.tmpdir(), `oam-conf-ipckill-${process.pid}.cjs`);
  writeFileSync(sleeper, "setTimeout(()=>process.exit(9), 30000);");
  const r = await new Promise((done) => {
    const cp = spawn(process.execPath, [sleeper], {
      stdio: ["ignore", "pipe", "pipe", "pipe", "ipc"],
    });
    // Immediately -- before any handle can have resolved on the deferred
    // launch. The child never connects its channel; teardown-on-exit must
    // still release the listener or 'close' never fires.
    cp.kill("SIGTERM");
    cp.on("close", (code, signal) => done(`ipc-kill code=${code} signal=${signal}`));
    setTimeout(() => done("ipc-kill TIMEOUT"), 8000);
  });
  console.log(r);
  rmSync(sleeper, { force: true });
}
