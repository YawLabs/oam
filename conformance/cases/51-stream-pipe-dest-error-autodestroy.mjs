// A piped destination that emits 'error' must have that error routed through
// pipe()'s internal handler: the pipe is torn down and -- when the dest has no
// other 'error' listener and uses autoDestroy -- the dest is destroyed (its
// custom destroy() fires) and the error is NOT re-thrown uncaught. Mirrors the
// tail of Node's test/parallel/test-stream-auto-destroy.js (Readable and
// Writable dest variants).
import { Readable, Writable } from "node:stream";

const log = [];
process.on("uncaughtException", (e) => { log.push("UNCAUGHT:" + (e && e.message)); });

await new Promise((resolve) => {
  const r2 = new Readable({
    autoDestroy: true,
    destroy(err, cb) { log.push("r2-destroy:" + (err && err.message)); cb(); },
  });
  const r = new Readable({ read() { r2.emit("error", new Error("fail")); } });
  r2.on("close", () => { log.push("r2-close"); resolve(); });
  r.pipe(r2);
});

await new Promise((resolve) => {
  const w = new Writable({
    autoDestroy: true,
    write(_d, _e, cb) { cb(); },
    destroy(err, cb) { log.push("w-destroy:" + (err && err.message)); cb(); },
  });
  const r = new Readable({ read() { w.emit("error", new Error("fail")); } });
  w.on("close", () => { log.push("w-close"); resolve(); });
  r.pipe(w);
});

console.log(log.join("\n"));
