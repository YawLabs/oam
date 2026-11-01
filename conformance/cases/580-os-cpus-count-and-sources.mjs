// What os.cpus(), os.loadavg() and os.uptime() report, pinned against node on
// the same host where node and oam read the same source. Cases 576-578 pin
// shapes, types and invariants; none of them would notice a wrong count, a
// mangled Windows model string, or a unix loadavg back at the zeros #441
// removed.
//
// - The CPU count, everywhere: node and oam both list the OS's online CPUs
//   (Windows: one SystemProcessorPerformanceInformation record each; macOS:
//   host_processor_info rows; Linux: /proc/stat's cpuN lines). The count is a
//   worker-pool size in most programs.
// - Windows only: each CPU's model and speed exactly. Both runtimes read the
//   same registry values (ProcessorNameString verbatim, ~MHz), which do not
//   change while the machine runs. Linux and macOS are left out on purpose:
//   oam's speed fallback there is a documented divergence
//   (docs/node-divergences.md, os rows).
// - Linux only: loadavg agrees with /proc/loadavg and uptime with
//   /proc/uptime -- the sources libuv reads -- within a tolerance, printed as
//   booleans so the two runtimes' lines are identical.
import fs from "node:fs";
import os from "node:os";

const cpus = os.cpus();
console.log("count", cpus.length);

if (process.platform === "win32") {
  console.log("models+speeds", JSON.stringify(cpus.map((c) => [c.model, c.speed])));
}

if (process.platform === "linux") {
  const proc = fs.readFileSync("/proc/loadavg", "utf8").trim().split(/\s+/).slice(0, 3).map(Number);
  const avg = os.loadavg();
  console.log("loadavg matches /proc/loadavg", avg.every((v, i) => Math.abs(v - proc[i]) <= 0.05));
  const before = Number(fs.readFileSync("/proc/uptime", "utf8").split(/\s+/)[0]);
  const up = os.uptime();
  const after = Number(fs.readFileSync("/proc/uptime", "utf8").split(/\s+/)[0]);
  console.log("uptime is /proc/uptime", up >= before - 0.01 && up <= after + 0.01);
}
