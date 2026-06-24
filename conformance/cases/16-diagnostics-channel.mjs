// diagnostics_channel: channel pub/sub + the TracingChannel trace API
// (traceSync / tracePromise / traceCallback). Regression guard for the
// ioredis-on-oam gap where tracingChannel() lacked tracePromise.
import dc from "node:diagnostics_channel";

// Plain channel pub/sub.
const ch = dc.channel("oam:test");
let received = null;
const onMsg = (msg) => { received = msg.v; };
ch.subscribe(onMsg);
console.log("hasSubscribers", ch.hasSubscribers, dc.hasSubscribers("oam:test"));
ch.publish({ v: 7 });
console.log("received", received);
ch.unsubscribe(onMsg);
console.log("afterUnsub", ch.hasSubscribers);

// TracingChannel: no-subscriber fast path just runs fn.
const tc = dc.tracingChannel("oam:trace");
const r1 = await tc.tracePromise(async (x) => x * 2, {}, null, 21);
console.log("nosub.tracePromise", r1);
const r2 = tc.traceSync((a, b) => a + b, {}, null, 2, 3);
console.log("nosub.traceSync", r2);
const r3 = await new Promise((resolve) =>
  tc.traceCallback((cb) => setTimeout(() => cb(null, "cbval"), 1), 0, {}, null, (e, v) => resolve(v)),
);
console.log("nosub.traceCallback", r3);

// With subscribers: channel publish ordering for tracePromise.
const seen = [];
tc.subscribe({
  start: () => seen.push("start"),
  end: () => seen.push("end"),
  asyncStart: () => seen.push("asyncStart"),
  asyncEnd: () => seen.push("asyncEnd"),
});
const r4 = await tc.tracePromise(async () => "ok", {});
console.log("sub.tracePromise", r4, seen.join(","));
console.log("hasSubscribers", tc.hasSubscribers);
