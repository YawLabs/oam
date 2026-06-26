// Node's public stream constructors are callable WITHOUT `new` (factory form):
// `stream.Writable({...})` returns an instance. oam wraps the five exports in a
// Proxy so the no-new call works while preserving instanceof, subclassing
// (class X extends stream.Writable -- used by fs/http/http2/tls/zlib), and the
// static factories (Readable.from / Duplex.from).
import { Readable, Writable, Duplex, Transform, PassThrough } from "node:stream";

// 1. no-new factory call.
const w = Writable({ write(_c, _e, cb) { cb(); } });
console.log("nonew-instanceof=" + (w instanceof Writable));
console.log("nonew-haswrite=" + (typeof w.write === "function"));

// 2. new with a subclass through the (proxied) export.
class MyWritable extends Writable {
  constructor(o) {
    super(o);
    this.mine = true;
  }
}
const mw = new MyWritable({ write(_c, _e, cb) { cb(); } });
console.log("sub-instanceof-base=" + (mw instanceof Writable));
console.log("sub-instanceof-self=" + (mw instanceof MyWritable));
console.log("sub-ctor-ran=" + (mw.mine === true));

// 3. statics survive the Proxy.
console.log("readable-from-isfn=" + (typeof Readable.from === "function"));
console.log("duplex-from-isfn=" + (typeof Duplex.from === "function"));
const rf = Readable.from([1, 2, 3]);
console.log("readable-from-instanceof=" + (rf instanceof Readable));

// 4. Transform / PassThrough no-new + prototype chain.
const pt = PassThrough();
console.log("pt-instanceof-passthrough=" + (pt instanceof PassThrough));
console.log("pt-instanceof-transform=" + (pt instanceof Transform));
console.log("pt-instanceof-duplex=" + (pt instanceof Duplex));
