// node:stream: Transform pipeline, object mode, finished, async iteration.
import { Readable, Writable, Transform, PassThrough } from "node:stream";
import { pipeline, finished } from "node:stream/promises";

const upper = new Transform({
  transform(chunk, _e, cb) {
    cb(null, chunk.toString().toUpperCase());
  },
});
let out = "";
const sink = new Writable({
  write(c, _e, cb) {
    out += c;
    cb();
  },
});
await pipeline(Readable.from(["ab", "cd"]), upper, new PassThrough(), sink);
console.log(out);

const objects = [];
for await (const v of Readable.from([{ n: 1 }, { n: 2 }])) objects.push(v.n);
console.log(objects.join(","));

let caught = "";
try {
  await pipeline(
    Readable.from(["x"]),
    new Transform({
      transform(_c, _e, cb) {
        cb(new Error("mid-fail"));
      },
    }),
    new Writable({
      write(_c, _e, cb) {
        cb();
      },
    }),
  );
} catch (e) {
  caught = e.message;
}
console.log(caught);

const w = new Writable({
  write(_c, _e, cb) {
    cb();
  },
});
const done = finished(w);
w.end("last");
await done;
console.log("finished-ok", w.writableFinished);
