// zlib: round trips, formats, unzip auto-detect, async forms, streams,
// and INTEROP -- a gzip blob produced by real Node must decompress here.
// Also: incremental streaming (wave-2): createGzip emits multiple chunks
// on a large input (not one-shot), and round-trips cleanly.
import zlib from "node:zlib";
import { Readable, Writable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { promisify } from "node:util";

const data = Buffer.from("oam conformance payload: " + "abc123".repeat(20));
console.log(zlib.gunzipSync(zlib.gzipSync(data)).equals(data));
console.log(zlib.inflateSync(zlib.deflateSync(data)).equals(data));
console.log(zlib.inflateRawSync(zlib.deflateRawSync(data)).equals(data));
console.log(zlib.unzipSync(zlib.gzipSync(data)).equals(data), zlib.unzipSync(zlib.deflateSync(data)).equals(data));
// Level effects size deterministically.
const best = zlib.gzipSync(data, { level: 9 });
const none = zlib.gzipSync(data, { level: 0 });
console.log(best.length < none.length, none.length > data.length);
// Node-produced vector (interop receipt).
const nodeVector = "H4sIAAAAAAAACstPzFVIzs9Lyy/KTcxLTlUoSKzMyU9MsVJITEo2NDKmPwkATCxew5EAAAA=";
console.log(zlib.gunzipSync(Buffer.from(nodeVector, "base64")).toString() === data.toString());
// Async callback form via promisify.
const gunzip = promisify(zlib.gunzip);
console.log((await gunzip(zlib.gzipSync(data))).equals(data));
// Transform classes through a pipeline.
const chunks = [];
await pipeline(
  Readable.from([data.subarray(0, 50), data.subarray(50)]),
  zlib.createGzip(),
  zlib.createGunzip(),
  new Writable({
    write(c, _e, cb) {
      chunks.push(c);
      cb();
    },
  }),
);
console.log(Buffer.concat(chunks).equals(data));
// Corrupt input fails loud.
let failed = false;
try {
  zlib.gunzipSync(Buffer.from("not gzip"));
} catch {
  failed = true;
}
console.log(failed);
// Incremental streaming: a 2 MB payload through createGzip must produce
// at least 2 distinct chunks (i.e. it is NOT buffering the whole input).
const LARGE = 2 * 1024 * 1024;
const large = Buffer.alloc(LARGE);
for (let i = 0; i < LARGE; i++) large[i] = i & 0xff;
let gzChunks = 0;
const gzBufs = [];
await pipeline(
  Readable.from([large]),
  zlib.createGzip(),
  new Writable({
    write(c, _e, cb) { gzChunks++; gzBufs.push(c); cb(); },
  }),
);
const gzOut = Buffer.concat(gzBufs);
const gunzBufs = [];
await pipeline(
  Readable.from([gzOut]),
  zlib.createGunzip(),
  new Writable({
    write(c, _e, cb) { gunzBufs.push(c); cb(); },
  }),
);
const gunzOut = Buffer.concat(gunzBufs);
// At least 2 chunks: incremental, not one-shot.
console.log(gzChunks >= 2);
// Round-trip fidelity.
console.log(gunzOut.equals(large));
