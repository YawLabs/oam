// node:zlib validates every engine's options in its constructors, and the
// one-shot forms construct an engine before they look at their input: the
// Zlib constructor checks windowBits (0 or none reads the window from the
// stream on the inflate side; a gzip deflater needs 9 or more), level,
// memLevel, strategy and dictionary, then ZlibBase checks chunkSize, flush,
// finishFlush and maxOutputLength. Brotli checks its params (key numbers
// 0..8, each once; number or boolean values; the decoder's library refuses
// all but 0 and 1) instead of the zlib options. A sync form then names its
// input "buffer", a callback form checks the callback and then the input as
// the stream's "chunk". `info: true` answers { buffer, engine }.
//
// Regression guard: oam read `level` and passed anything through, so
// `deflateSync('x', { level: 99 })` returned data where node throws
// ERR_OUT_OF_RANGE; no constructor checked level, memLevel, strategy,
// windowBits or chunkSize; brotli params went unread; the one-shot forms took
// a number as input (`new Uint8Array(5)`) and ignored `info`. Then the
// input check threw for null and undefined in the callback forms, which node
// runs on empty input (its engine's end() writes nothing for either), and a
// brotli stream that stopped short failed with a plain Error, not node's
// Z_BUF_ERROR "unexpected end of file".
//
// Prints error class, code and message, and never a compressed byte.
import zlib from "node:zlib";

const describe = (e) => `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)}`;
const attempt = (label, fn) => {
  try {
    const out = fn();
    const shown = out === undefined
      ? "undefined"
      : Buffer.isBuffer(out)
        ? `buffer ${out.length}`
        : out.constructor.name;
    console.log(`${label}: ok ${shown}`);
  } catch (e) {
    console.log(`${label}: ${describe(e)}`);
  }
};
const show = (v) =>
  typeof v === "bigint" ? `${v}n` : typeof v === "string" ? JSON.stringify(v) : String(v);

const classes = ["Deflate", "Inflate", "DeflateRaw", "InflateRaw", "Gzip", "Gunzip", "Unzip"];

// level, memLevel, strategy, chunkSize: one range for every zlib class.
const values = [undefined, null, NaN, Infinity, "5", 5n, -2, -1, 0, 1, 0.5, 4, 5, 9, 9.5, 10, 63, 64];
for (const key of ["level", "memLevel", "strategy", "chunkSize"]) {
  for (const value of values) {
    attempt(`Deflate ${key} ${show(value)}`, () => new zlib.Deflate({ [key]: value }));
  }
}
for (const name of classes) {
  attempt(`${name} level 99`, () => new zlib[name]({ level: 99 }));
}

// windowBits differs by class: the inflate side takes 0 and null, gzip needs 9.
for (const name of classes) {
  for (const value of [undefined, null, 0, 7, 8, 9, 15, 16, 9.5]) {
    attempt(`${name} windowBits ${show(value)}`, () => new zlib[name]({ windowBits: value }));
  }
}

// Order: each check before the next.
const bad = {
  windowBits: 99, level: 99, memLevel: 99, strategy: 99, dictionary: 5,
  chunkSize: 1, flush: 99, finishFlush: 99, maxOutputLength: 0,
};
const keys = Object.keys(bad);
for (let i = 0; i < keys.length; i++) {
  const options = {};
  for (const key of keys.slice(i)) options[key] = bad[key];
  attempt(`from ${keys[i]}`, () => new zlib.Inflate(options));
}

// Every way in: the factories, the .call(this) path, the one-shot forms.
attempt("createGzip level 10", () => zlib.createGzip({ level: 10 }));
attempt("Inflate.call chunkSize 1", () => zlib.Inflate.call({}, { chunkSize: 1 }));
attempt("Inflate.call level -2", () => zlib.Inflate.call({}, { level: -2 }));
attempt("deflateSync level 99", () => zlib.deflateSync("x", { level: 99 }));
attempt("gzipSync windowBits 8", () => zlib.gzipSync("x", { windowBits: 8 }));
attempt("deflateRawSync windowBits 8", () => zlib.deflateRawSync("x", { windowBits: 8 }));
attempt("inflateSync memLevel 0", () => zlib.inflateSync(zlib.deflateSync("x"), { memLevel: 0 }));
attempt("unzipSync strategy 5", () => zlib.unzipSync(zlib.gzipSync("x"), { strategy: 5 }));
attempt("deflate level 99", () => zlib.deflate("x", { level: 99 }, () => {}));
attempt("gunzip chunkSize 63", () => zlib.gunzip(zlib.gzipSync("x"), { chunkSize: 63 }, () => {}));

// Options before input; the input's name and types per form.
attempt("deflateSync 5 level 99", () => zlib.deflateSync(5, { level: 99 }));
attempt("deflateSync 5", () => zlib.deflateSync(5));
attempt("deflateSync object", () => zlib.deflateSync({}));
attempt("deflateSync ArrayBuffer", () => zlib.inflateSync(zlib.deflateSync(new ArrayBuffer(3))));
attempt("deflateSync options 5", () => zlib.inflateSync(zlib.deflateSync("x", 5)));
attempt("deflate no callback", () => zlib.deflate("x", {}));
attempt("deflate callback 5", () => zlib.deflate(5, {}, 5));
attempt("deflate input 5", () => zlib.deflate(5, {}, () => {}));
attempt("deflate options null", () => zlib.deflate("x", null, () => {}));

// dictionary: a view or an ArrayBuffer, for every class.
for (const name of classes) {
  for (const [label, value] of [["null", null], ["string", "abc"], ["array", [1]], ["ArrayBuffer", new ArrayBuffer(2)], ["DataView", new DataView(new ArrayBuffer(2))]]) {
    attempt(`${name} dictionary ${label}`, () => new zlib[name]({ dictionary: value }));
  }
}

// Brotli: params keys and values, then ZlibBase's checks with brotli's flush range.
const params = [
  { 0: 0 }, { 1: 11 }, { 1: 99 }, { 2: 22 }, { 8: 1 }, { 9: 1 }, { "-1": 1 }, { abc: 1 },
  { "1.5": 1 }, { "": 1 }, { 1: 3, "0x1": 5 }, { 1: -1, "0x1": 5 }, { 1: "5" }, { 1: true },
  { 1: null }, { 1: 5n }, { 2: -1 }, { 2: NaN },
];
for (const name of ["BrotliCompress", "BrotliDecompress"]) {
  for (const p of params) {
    attempt(`${name} params ${JSON.stringify(p, (k, v) => typeof v === "bigint" ? `${v}n` : v)}`, () => new zlib[name]({ params: p }));
  }
  attempt(`${name} params 'ab'`, () => new zlib[name]({ params: "ab" }));
  attempt(`${name} level 99`, () => new zlib[name]({ level: 99 }));
  attempt(`${name} flush 3`, () => new zlib[name]({ flush: 3 }));
  attempt(`${name} flush 4`, () => new zlib[name]({ flush: 4 }));
  attempt(`${name} finishFlush 4`, () => new zlib[name]({ finishFlush: 4 }));
  attempt(`${name} chunkSize 1`, () => new zlib[name]({ chunkSize: 1 }));
  attempt(`${name} params {2:1} chunkSize 1`, () => new zlib[name]({ params: { 2: 1 }, chunkSize: 1 }));
  attempt(`${name} maxOutputLength 0`, () => new zlib[name]({ maxOutputLength: 0 }));
}
attempt("brotliCompress params 99", () => zlib.brotliCompress("x", { params: { 99: 1 } }, () => {}));
attempt("brotliCompress no callback", () => zlib.brotliCompress("x", {}));
attempt("brotliCompress input 5", () => zlib.brotliCompress(5, {}, () => {}));
attempt("brotliCompressSync params 99", () => zlib.brotliCompressSync("x", { params: { 99: 1 } }));
attempt("brotliDecompressSync params 2", () => zlib.brotliDecompressSync("x", { params: { 2: 1 } }));
attempt("brotliCompressSync input 5", () => zlib.brotliCompressSync(5));

// null and undefined are no input to a callback form: node ends its engine
// with them, and end() writes nothing for either, so the call runs on empty
// input -- an empty stream out of a deflater, Z_BUF_ERROR out of an inflater.
// A sync form names them "buffer".
for (const input of [null, undefined]) {
  for (const name of ["deflate", "gzip", "deflateRaw", "inflate", "gunzip", "inflateRaw", "unzip", "brotliCompress", "brotliDecompress"]) {
    await new Promise((resolve) => {
      zlib[name](input, (err, out) => {
        console.log(`${name}(${input}): ${err ? `${describe(err)} ${err.errno}` : `ok ${out.length}`}`);
        resolve();
      });
    });
  }
  attempt(`deflateSync(${input})`, () => zlib.deflateSync(input));
  attempt(`inflateSync(${input})`, () => zlib.inflateSync(input));
}
await new Promise((resolve) => {
  zlib.brotliCompress("hello hello hello", (err, packed) => {
    zlib.brotliDecompress(packed.subarray(0, 5), (e) => {
      console.log(`brotliDecompress cut short: ${describe(e)} ${e.errno}`);
      const stream = zlib.createBrotliDecompress();
      stream.on("error", (e2) => {
        console.log(`BrotliDecompress stream cut short: ${describe(e2)} ${e2.errno}`);
        resolve();
      });
      stream.resume();
      stream.end(packed.subarray(0, 5));
    });
  });
});

// The engines and what they hold.
const d = new zlib.Deflate({ level: 3, strategy: 1, chunkSize: 100, info: "y" });
console.log("Deflate", d.constructor === zlib.Deflate, zlib.Deflate.name, d._level, d._strategy,
  d._chunkSize, d._defaultFlushFlag, d._finishFlushFlag, d._maxOutputLength, d._info, d.bytesWritten);
const g = new zlib.Gzip({ level: NaN });
console.log("Gzip", g._level, g._strategy);
console.log("BrotliCompress", zlib.BrotliCompress.name, new zlib.BrotliCompress()._level);

// info: { buffer, engine } from both forms.
const info = zlib.deflateSync("abc", { info: true });
console.log("sync info", Object.keys(info).join(","), zlib.inflateSync(info.buffer).toString(),
  info.engine.constructor.name, info.engine.bytesWritten);
console.log("sync info 0", Buffer.isBuffer(zlib.gzipSync("abc", { info: 0 })));
await new Promise((resolve) => {
  zlib.gzip("abcd", { info: 1 }, (err, res) => {
    console.log("async info", err, Object.keys(res).join(","), zlib.gunzipSync(res.buffer).toString(),
      res.engine.constructor.name, res.engine.bytesWritten);
    resolve();
  });
});
await new Promise((resolve) => {
  zlib.brotliCompress(Buffer.alloc(5000, 0x61), (err, packed) => {
    zlib.brotliDecompress(packed, { maxOutputLength: 4999 }, (e) => {
      console.log("brotli cap", describe(e));
      zlib.brotliDecompress(packed, { maxOutputLength: 5000 }, (e2, out) => {
        console.log("brotli at cap", e2, out.length);
        resolve();
      });
    });
  });
});
await new Promise((resolve) => {
  zlib.brotliCompress("abcde", { info: true }, (err, res) => {
    console.log("brotli info", err, Object.keys(res).join(","), res.engine.constructor.name,
      res.engine.bytesWritten);
    resolve();
  });
});
