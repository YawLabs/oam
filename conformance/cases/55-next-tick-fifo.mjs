// process.nextTick FIFO semantics (slice 0 of the streams port), asserted
// from SYNCHRONOUS code inside a macrotask callback -- the one context where
// the two runtimes are identical. There Node drains the tick queue ahead of
// promise jobs queued during the same callback: FIFO among ticks, mid-drain
// ticks join the batch, batch precedes the promise jobs. (Ticks scheduled
// from promise-job contexts -- .then/await continuations, ESM top-level --
// diverge by design: Node exhausts the whole microtask queue before
// re-draining ticks, oam's batch drains at its scheduling position.
// Documented residual, deliberately not asserted here.)
const order = [];
await new Promise((done) => {
  setTimeout(() => {
    process.nextTick(() => {
      order.push("t1");
      process.nextTick(() => order.push("t2"));
      Promise.resolve().then(() => order.push("p1"));
    });
    process.nextTick(() => order.push("t3"));
    Promise.resolve().then(() => order.push("p0"));
    queueMicrotask(() => order.push("m0"));
    setTimeout(() => {
      console.log(order.join(","));
      done();
    }, 5);
  }, 0);
});
