// os.uptime(): seconds since the system booted, as libuv's uv_uptime reports
// them. oam answered with the PROCESS uptime (the clock behind
// process.uptime()), so a freshly started oam reported an uptime of
// milliseconds on a machine that had been up for days.
//
// The value is time-varying, so only its relations are printed: finite and
// positive, at least the process's own uptime, non-decreasing across a spin,
// and whole seconds on macOS (libuv subtracts the boot time's seconds there;
// Windows and Linux carry a fraction).
import os from "node:os";

const a = os.uptime();
const p = process.uptime();
const t0 = performance.now();
while (performance.now() - t0 < 20) {
  // spin
}
const b = os.uptime();

console.log("number", typeof a === "number" && Number.isFinite(a));
console.log("positive", a > 0);
console.log("at-least-process", a >= p);
console.log("monotone", b >= a);
console.log("darwin-whole-seconds", process.platform !== "darwin" || Number.isInteger(a));
