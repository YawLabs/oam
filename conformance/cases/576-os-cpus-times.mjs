// os.cpus(): node's entry shape and the cumulative per-CPU time counters
// (#441). oam hard-coded every `times` field to 0 on every platform, so a
// sampler that differences two calls (busy = 1 - idle delta / total delta)
// divided by zero and never read a value.
//
// Only what both runtimes share is printed. `model` and `speed` are checked
// for type alone: node's speed is 0 where cpufreq is absent and oam falls
// back to /proc/cpuinfo's "cpu MHz" there, node names an aarch64 core only
// when libuv's part table lists it, and node reports a 2400 MHz placeholder
// on Apple Silicon (see docs/node-divergences.md). The counters themselves
// are cumulative since boot, so they are compared across two samples, not
// printed. The spin runs on performance.now() (monotonic in both runtimes);
// the wall clock can step.
import fs from "node:fs";
import os from "node:os";

const FIELDS = ["user", "nice", "sys", "idle", "irq"];
const sum = (cpus) => cpus.reduce((n, c) => FIELDS.reduce((m, k) => m + c.times[k], n), 0);

// Linux cross-check: the kernel's own counters, read around the call. libuv
// reads these columns of /proc/stat (user, nice, system, idle, skip iowait,
// irq) in 10 ms ticks, so each field node or oam reports lies between the
// pre- and post-read tick counts times 10. The five counters are monotone
// (only iowait, which libuv skips, can decrease), so the sandwich holds on
// a loaded host too. Elsewhere the line prints true.
const procStat = () =>
  process.platform === "linux"
    ? fs
        .readFileSync("/proc/stat", "utf8")
        .split("\n")
        .filter((line) => /^cpu\d/.test(line))
        .map((line) => {
          const [label, ...cols] = line.trim().split(/\s+/);
          const n = cols.map(Number);
          return { cpu: Number(label.slice(3)), user: n[0], nice: n[1], sys: n[2], idle: n[3], irq: n[5] };
        })
        .sort((a, b) => a.cpu - b.cpu)
    : null;

const pre = procStat();
const a = os.cpus();
const post = procStat();
const t0 = performance.now();
while (performance.now() - t0 < 150) {
  // spin: at least one CPU accrues user time, and the others accrue idle
}
const b = os.cpus();

console.log("count", a.length > 0 && a.length === b.length);
console.log("entry-keys", JSON.stringify(Object.keys(a[0])));
console.log("times-keys", JSON.stringify(Object.keys(a[0].times)));
console.log("types", a.every((c) => typeof c.model === "string" && typeof c.speed === "number"));
console.log(
  "integers",
  [...a, ...b].every((c) => FIELDS.every((k) => Number.isInteger(c.times[k]) && c.times[k] >= 0)),
);
console.log("aggregate>0", sum(a) > 0);
console.log(
  "monotone",
  a.length === b.length && a.every((c, i) => FIELDS.every((k) => b[i].times[k] >= c.times[k])),
);
console.log("advanced", sum(b) > sum(a));
console.log("fresh-objects", a !== b && a[0] !== b[0] && a[0].times !== b[0].times);
// libuv has no nice counter on Windows and no irq counter on macOS.
console.log("win32-nice", process.platform !== "win32" || a.every((c) => c.times.nice === 0));
console.log("darwin-irq", process.platform !== "darwin" || a.every((c) => c.times.irq === 0));
console.log(
  "linux-proc-stat",
  process.platform !== "linux" ||
    (pre.length === a.length &&
      post.length === a.length &&
      a.every((c, i) =>
        FIELDS.every((k) => pre[i][k] * 10 <= c.times[k] && c.times[k] <= post[i][k] * 10),
      )),
);
