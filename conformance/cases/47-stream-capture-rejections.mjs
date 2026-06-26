// EventEmitter captureRejections: a stream built with { captureRejections: true }
// routes a rejecting async listener to destroy(err) (which then emits 'error'),
// instead of surfacing an unhandled rejection. Also exercises that a Writable
// emits 'drain' once per backed-up write (Node afterWrite emits 'drain' before
// the chunk callback, so a write() issued from that callback earns its own drain).
import { Readable, Writable } from "node:stream";

const log = [];

await new Promise((resolve) => {
  const r = new Readable({ captureRejections: true, read() {} });
  r.push("hello");
  const err = new Error("kaboom");
  r.on("error", (e) => {
    log.push(`R: error===err ${e === err} destroyed ${r.destroyed}`);
    resolve();
  });
  r.on("data", async () => {
    throw err;
  });
});

await new Promise((resolve) => {
  const w = new Writable({
    captureRejections: true,
    highWaterMark: 1,
    write(chunk, enc, cb) {
      process.nextTick(cb);
    },
  });
  const err = new Error("kaboom");
  let drains = 0;
  w.write("hello", () => {
    w.write("world");
  });
  w.on("error", (e) => {
    log.push(`W: drains ${drains} error===err ${e === err} destroyed ${w.destroyed}`);
    resolve();
  });
  w.on("drain", async () => {
    drains++;
    throw err;
  });
});

console.log(log.join("\n"));
