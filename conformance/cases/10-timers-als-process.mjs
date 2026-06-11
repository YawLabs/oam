// Scheduling order, AsyncLocalStorage propagation, process shape.
import { AsyncLocalStorage } from "node:async_hooks";

const order = [];
process.nextTick(() => order.push("tick"));
queueMicrotask(() => order.push("micro"));
setTimeout(() => order.push("t0"), 0);
setTimeout(() => order.push("t5"), 5);
await new Promise((resolve) => setTimeout(resolve, 20));
// tick-vs-micro RELATIVE order is a documented oam divergence (nextTick is
// microtask-based); what both runtimes guarantee: both fire before timers,
// and the timers fire in deadline order.
console.log(order.slice(0, 2).sort().join(","), order.slice(2).join(","));

const als = new AsyncLocalStorage();
const seen = [];
await als.run("ctx", async () => {
  await Promise.resolve();
  seen.push(als.getStore());
  await new Promise((resolve) => setTimeout(resolve, 1));
  seen.push(als.getStore());
});
seen.push(als.getStore());
console.log(seen.join(","));

console.log(
  ["win32", "darwin", "linux"].includes(process.platform),
  typeof process.cwd() === "string",
  typeof process.pid === "number",
  Array.isArray(process.argv),
  typeof process.env === "object",
);
process.exitCode = 0;
