// Stream `destroyed` is a read/write accessor: assigning `stream.destroyed =
// true` flips the underlying state flag (Node-faithful back-compat setter) and
// the getter reads it back as true. Mirrors the `duplex.destroyed = true` path
// exercised by Node's test-stream-duplex-destroy. On a fresh stream the setter
// only marks the flag -- no 'close'/'error' is emitted, so output is
// deterministic.
import { Readable, Writable, Duplex } from "node:stream";

const r = new Readable({ read() {} });
r.destroyed = true;
console.log("readable", r.destroyed);

const w = new Writable({ write(c, e, cb) { cb(); } });
w.destroyed = true;
console.log("writable", w.destroyed);

const d = new Duplex({ read() {}, write(c, e, cb) { cb(); } });
d.destroyed = true;
console.log("duplex", d.destroyed);
