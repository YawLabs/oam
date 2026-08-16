// A child that NEVER STARTS on the extra-fd path -- what spawn() owes its
// caller when stdio names fds beyond 2 and the binary does not exist.
//
// Case 64 pinned the failure contract for the PLAIN path ('error' first, then
// 'close' carrying the libuv errno). The extra-fd backend shares none of that
// code: its spawn failure comes out of a different native op and a different
// catch, which emitted 'error' and nothing else -- so a CDP-style caller whose
// completion path is 'close' (probing for a browser binary that is not
// installed, THE canonical extra-fd spawn failure) stalled forever. The raw
// native backends also omitted `errno` from the failure body, so even with a
// 'close' the code argument could not have been node's.
//
// Platform-gated like case 63: the extra-fd backend exists on
// win32/linux/darwin; elsewhere the same expected lines are printed so
// node==oam still holds. The errno constant in the unsupported branch is
// arbitrary but shared, for the same reason.
import { spawn } from "node:child_process";

const supported =
  process.platform === "win32" || process.platform === "linux" || process.platform === "darwin";

const MISSING = "oam-no-such-binary-zzz";

if (!supported) {
  console.log("error ENOENT syscall spawn oam-no-such-binary-zzz");
  console.log("close -2 null");
} else {
  const r = await new Promise((done) => {
    const cp = spawn(MISSING, ["--flag"], {
      stdio: ["ignore", "pipe", "pipe", "pipe", "pipe"],
    });
    const seen = { err: "NEVER", close: "NEVER" };
    cp.on("error", (e) => {
      seen.err = `${e.code} syscall ${e.syscall}`;
    });
    cp.on("close", (code, signal) => {
      seen.close = `${code} ${signal}`;
      // 'close' is the completion signal; resolving here is itself the proof
      // it fired ('error' alone leaves seen.close at NEVER via the timeout).
      done(seen);
    });
    setTimeout(() => done(seen), 4000);
  });
  console.log("error", r.err);
  console.log("close", r.close);
}
