// node 26's perf_hooks top-level timerify / eventLoopUtilization and
// createSlidingWindowHistogram.
//
// timerify: the wrapper's fixed name / length, call and construct
// forwarding, a returned promise passed through, histogram recording, and
// the argument checks. createSlidingWindowHistogram: the options checks and
// a records-per-chunk window's rotation (count, min, max after each
// record), reset, the snapshot's class, and the record checks. Time-based
// windows and timing values are left out: they are not deterministic.
import ph from "node:perf_hooks";

const show = (label, fn) => {
  try {
    console.log(label, fn());
  } catch (e) {
    console.log(label, "threw", e.name, e.code, e.message);
  }
};

console.log(ph.timerify === ph.performance.timerify, ph.eventLoopUtilization === ph.performance.eventLoopUtilization);
console.log(typeof ph.createSlidingWindowHistogram);
function add(a, b) {
  return a + b;
}
const t = ph.timerify(add);
console.log(t.name, t.length, t(1, 2), JSON.stringify(Object.getOwnPropertyDescriptor(t, "name")));
const h = ph.createHistogram();
const th = ph.timerify(add, { histogram: h });
th(1, 2);
th(3, 4);
console.log("recorded", h.count, h.min > 0);
class K {
  constructor(x) {
    this.x = x;
  }
}
const TK = ph.timerify(K);
const k = new TK(5);
console.log("construct", k instanceof K, k.x);
const tp = ph.timerify(async (x) => x * 2);
console.log("promise", await tp(21));
show("fn type", () => ph.timerify(5));
show("options type", () => ph.timerify(() => {}, 5));
show("histogram type", () => ph.timerify(() => {}, { histogram: {} }));

show("no options", () => ph.createSlidingWindowHistogram());
show("no chunks", () => ph.createSlidingWindowHistogram({}));
show("neither", () => ph.createSlidingWindowHistogram({ chunks: 2 }));
show("both", () => ph.createSlidingWindowHistogram({ chunks: 2, chunkDuration: 10, recordsPerChunk: 3 }));
show("chunks 0", () => ph.createSlidingWindowHistogram({ chunks: 0, recordsPerChunk: 3 }));
show("chunks 1025", () => ph.createSlidingWindowHistogram({ chunks: 1025, recordsPerChunk: 3 }));
show("records 0", () => ph.createSlidingWindowHistogram({ chunks: 2, recordsPerChunk: 0 }));
show("duration 1.5", () => ph.createSlidingWindowHistogram({ chunks: 2, chunkDuration: 1.5 }));
show("highest < 2*lowest", () => ph.createSlidingWindowHistogram({ chunks: 2, recordsPerChunk: 1, lowest: 10, highest: 15 }));

const w = ph.createSlidingWindowHistogram({ chunks: 3, recordsPerChunk: 2 });
console.log(w.constructor.name, Object.getOwnPropertyNames(Object.getPrototypeOf(w)).join(","));
for (let i = 1; i <= 10; i++) {
  w.record(i);
  const snap = w.snapshot();
  console.log(i, snap.count, snap.min, snap.max);
}
const snap = w.snapshot();
console.log(snap.constructor.name, typeof snap.record);
w.reset();
const empty = w.snapshot();
console.log("reset", empty.count, empty.min, empty.max, empty.mean);
show("record 0", () => (w.record(0), w.snapshot().min));
show("record bigint", () => (w.record(5n), w.snapshot().max));
show("record -1", () => w.record(-1));
show("record 1.5", () => w.record(1.5));
show("record string", () => w.record("1"));
show("constructor", () => new w.constructor());
show("brand", () => w.constructor.prototype.record.call({}, 1));
