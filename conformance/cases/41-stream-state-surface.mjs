// Stream internal-state surface: Node exposes _readableState / _writableState
// with the field names the ecosystem reads directly. Guards oam's alias of
// those onto its internal state + the writableNeedDrain / push-after-EOF /
// write-validation behavior.
import { Readable, Writable } from "node:stream";

const tag = (label, fn) => {
  try {
    const v = fn();
    console.log(label + "=" + v);
  } catch (e) {
    console.log(label + ":throw:" + (e.code || e.name));
  }
};

// Writable state fields the corpus reads.
const w = new Writable({ highWaterMark: 1, write(_c, _e, cb) { /* never cb */ } });
console.log("w.needDrain.init=" + w._writableState.needDrain);
console.log("w.objectMode=" + w._writableState.objectMode);
console.log("w.highWaterMark=" + w._writableState.highWaterMark);
console.log("w.corked.init=" + w._writableState.corked);
w.write("aaaa");
console.log("w.needDrain.after=" + w._writableState.needDrain);
console.log("w.writableNeedDrain=" + w.writableNeedDrain);
w.cork();
console.log("w.corked.after=" + w._writableState.corked);

// Write validation (synchronous throws, Node-coded).
tag("write-null", () => { w.write(null); return "nothrow"; });
tag("write-object", () => { w.write({}); return "nothrow"; });

// Readable state fields.
const r = new Readable({ read() {} });
console.log("r.objectMode=" + r._readableState.objectMode);
console.log("r.flowing.init=" + r._readableState.flowing);
console.log("r.pipes.isarray=" + Array.isArray(r._readableState.pipes));
console.log("r.length.init=" + r._readableState.length);
r.push("hello");
console.log("r.length.after=" + r._readableState.length);

// push() after EOF -> async ERR_STREAM_PUSH_AFTER_EOF.
const r2 = new Readable({ read() {} });
r2.on("error", (e) => console.log("push-after-eof=" + e.code));
r2.push(null);
r2.push("x");

// readableListening reflects the 'readable' listener set.
const r3 = new Readable({ read() {} });
console.log("readableListening.before=" + r3._readableState.readableListening);
r3.on("readable", () => {});
console.log("readableListening.after=" + r3._readableState.readableListening);
