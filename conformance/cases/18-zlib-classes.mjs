// zlib streaming CLASS constructors: exported, callable, inheritable.
// Regression guard for the pngjs/playwright gap where zlib.Inflate (the
// class) was undefined, so `util.inherits(MyInflate, zlib.Inflate)` threw.
import zlib from "node:zlib";
import util from "node:util";
import { Transform } from "node:stream";

const inst = new zlib.Inflate();
console.log("surface", typeof zlib.Inflate, inst instanceof zlib.Inflate, inst instanceof Transform, typeof inst.on === "function");
console.log("classes", typeof zlib.Deflate, typeof zlib.Gzip, typeof zlib.Gunzip, typeof zlib.InflateRaw, typeof zlib.BrotliCompress);
console.log("constants", zlib.Z_MIN_CHUNK, zlib.constants.Z_MIN_CHUNK, zlib.Z_DEFAULT_CHUNK);

// Transpiled-style inheritance: util.inherits + zlib.Inflate.call(this).
function Sub(opts) {
  if (!(this instanceof Sub)) return new Sub(opts);
  zlib.Inflate.call(this, opts);
}
util.inherits(Sub, zlib.Inflate);
const sub = new Sub();
console.log("inherits", sub instanceof Sub, sub instanceof zlib.Inflate, typeof sub.on === "function", typeof sub._handle, sub._chunkSize > 0);

// Streaming round-trip through the class-backed createGzip.
const data = Buffer.from("zlib classes round-trip payload ".repeat(40));
const gz = zlib.createGzip();
const parts = [];
gz.on("data", (c) => parts.push(c));
gz.on("end", () => {
  console.log("roundtrip", zlib.gunzipSync(Buffer.concat(parts)).equals(data));
});
gz.end(data);
