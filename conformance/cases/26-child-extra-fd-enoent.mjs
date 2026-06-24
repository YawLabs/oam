// Extra-fd spawn failure must surface a Node-shaped error: spawning a missing
// binary with a >3-entry stdio array (the extra-fd path) emits an 'error' event
// whose .code is "ENOENT" -- the field ecosystem code branches on. Regression
// guard for the native spawn-failure JSON {code,message} being lifted into
// err.code (rather than emitted as an opaque blob).
import { spawn } from "node:child_process";

const cp = spawn("oam-no-such-binary-zzz", [], {
  stdio: ["ignore", "pipe", "pipe", "pipe", "pipe"],
});
cp.on("error", (e) => {
  console.log("error", e.code);
});
