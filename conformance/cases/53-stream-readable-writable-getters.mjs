// Public stream state getters Node exposes that oam was missing: readableEncoding
// (null unless setEncoding), instance .errored (null until errored),
// readableDidRead (false until a chunk is read/emitted), and the readable/
// writableAborted shape. Values are byte-identical to Node v22.
import { Readable, Writable } from "node:stream";

const r = new Readable({ read() {} });
console.log("r.readableEncoding=" + r.readableEncoding);
console.log("r.errored=" + r.errored);
console.log("r.readableDidRead.before=" + r.readableDidRead);
console.log("r.readableAborted.before=" + r.readableAborted);

r.setEncoding("utf8");
console.log("r.readableEncoding.after=" + r.readableEncoding);

r.push("hello");
r.push(null);
console.log("r.readableDidRead.afterRead=" + (r.read() !== null) + ":" + r.readableDidRead);

const w = new Writable({ write(_c, _e, cb) { cb(); } });
console.log("w.errored=" + w.errored);
console.log("w.writableAborted.before=" + w.writableAborted);

// errored reflects a destroy(err).
const e = new Writable({ write(_c, _e, cb) { cb(); } });
e.on("error", () => {});
e.destroy(new Error("boom"));
queueMicrotask(() => {
  console.log("e.errored.code=" + (e.errored && e.errored.message));
});
