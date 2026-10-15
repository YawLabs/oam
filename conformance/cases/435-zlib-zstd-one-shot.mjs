// node:zlib's zstd one-shot API (node v22.22.2, libzstd 1.5.7): the exports
// and constants, zstdCompressSync / zstdDecompressSync, the callback forms and
// their util.promisify form, and what libzstd's streaming calls make of odd
// input under node's own one-shot loop -- a decompress returns only the FIRST
// frame and ignores what follows it, input that stops inside a frame decodes
// to what it held with no error, an empty input is an empty buffer -- with
// node's coded errors for what libzstd refuses.
//
// oam links the same libzstd node does, so the compressed bytes are compared
// too, not only round trips: they are libzstd 1.5.7's at the same level and
// parameters. The frames decoded below were made by node and are embedded as
// fixed bytes.
//
// Regression guard: oam had no zstd at all -- no exports, no constants, no
// process.versions.zstd -- though node added it in v22.15.0.
import zlib from "node:zlib";
import { createHash } from "node:crypto";
import { promisify } from "node:util";

const C = zlib.constants;
const hex = (b) => Buffer.from(b).toString("hex");
const digest = (b) => createHash("sha256").update(b).digest("hex").slice(0, 16);
const describe = (e) =>
  `${e.constructor.name} code=${e.code} errno=${e.errno} ${JSON.stringify(e.message)} ` +
  `own=${Object.getOwnPropertyNames(e).filter((k) => k !== "stack").join(",")}`;
const attempt = (label, fn) => {
  try {
    const out = fn();
    console.log(`${label}: ok ${Buffer.isBuffer(out) ? hex(out) : JSON.stringify(out)}`);
  } catch (e) {
    console.log(`${label}: ${describe(e)}`);
  }
};

console.log("versions.zstd:", process.versions.zstd);

// The exports: names, kinds, lengths and function names.
for (const name of Object.keys(zlib).filter((k) => /zstd/i.test(k))) {
  const f = zlib[name];
  console.log(`export ${name}: ${typeof f} length=${f.length} name=${f.name}`);
}
const created = Object.getOwnPropertyDescriptor(zlib, "createZstdCompress");
console.log(
  "createZstdCompress descriptor:",
  created.writable, created.enumerable, created.configurable,
);
// The constants, and their deprecated non-enumerable top-level copies.
const zstdKeys = Object.keys(C).filter((k) => k.startsWith("ZSTD"));
console.log("constants:", zstdKeys.map((k) => `${k}=${C[k]}`).join(" "));
const top = Object.getOwnPropertyDescriptor(zlib, "ZSTD_e_end");
console.log("top-level ZSTD_e_end:", top.value, top.enumerable, top.writable, top.configurable);
console.log("enumerable top-level ZSTD_*:", Object.keys(zlib).filter((k) => k.startsWith("ZSTD_")).length);

// Compressed bytes: libzstd's, as node's.
for (const [label, input, opts] of [
  ["hello", "hello hello hello", undefined],
  ["empty", "", undefined],
  ["one byte", "x", undefined],
  ["level 19", "hello hello hello", { params: { [C.ZSTD_c_compressionLevel]: 19 } }],
  ["level -5", "hello hello hello", { params: { [C.ZSTD_c_compressionLevel]: -5 } }],
  ["level 100 (clamped)", "hello hello hello", { params: { [C.ZSTD_c_compressionLevel]: 100 } }],
  ["checksum", "hello world", { params: { [C.ZSTD_c_checksumFlag]: 1 } }],
  ["checksum 5", "x", { params: { [C.ZSTD_c_checksumFlag]: 5 } }],
  ["no content size", "x", { params: { [C.ZSTD_c_contentSizeFlag]: 0 } }],
  ["strategy btultra2", "hello hello hello", { params: { [C.ZSTD_c_strategy]: C.ZSTD_btultra2 } }],
  ["windowLog 10", "hello hello hello", { params: { [C.ZSTD_c_windowLog]: 10 } }],
  ["-1 is unset", "x", { params: { [C.ZSTD_c_checksumFlag]: -1 } }],
  ["finishFlush continue", "hello", { finishFlush: C.ZSTD_e_continue }],
  ["finishFlush flush", "hello", { finishFlush: C.ZSTD_e_flush }],
  ["pledgedSrcSize ignored one-shot", "xx", { pledgedSrcSize: 5 }],
  ["Uint16Array", new Uint16Array([0x6968, 0x2121]), undefined],
  ["ArrayBuffer", new Uint8Array([104, 105]).buffer, undefined],
]) {
  attempt(`compress ${label}`, () => zlib.zstdCompressSync(input, opts));
}

// Larger inputs: several 16 KiB output windows, at a few levels.
const big = Buffer.alloc(300000);
for (let i = 0; i < big.length; i++) big[i] = ((i * 7919) >> 3) & 255;
const text = Buffer.from("the quick brown fox jumps over the lazy dog ".repeat(5000));
for (const [label, input] of [["mixed", big], ["text", text]]) {
  for (const level of [1, 3, 9, 19]) {
    const packed = zlib.zstdCompressSync(input, { params: { [C.ZSTD_c_compressionLevel]: level } });
    const back = zlib.zstdDecompressSync(packed);
    console.log(`${label} level ${level}: ${packed.length} ${digest(packed)} roundtrip=${back.equals(input)}`);
  }
}
// A small chunkSize: many output windows on both sides.
{
  const packed = zlib.zstdCompressSync(text, { chunkSize: 64 });
  const back = zlib.zstdDecompressSync(packed, { chunkSize: 64 });
  console.log(`chunkSize 64: ${packed.length} ${digest(packed)} roundtrip=${back.equals(text)}`);
}

// Frames node made, decoded.
const frames = {
  hello: "28b52ffd20116500003068656c6c6f200100314a11",
  a: "28b52ffd200109000061",
  b: "28b52ffd200109000062",
  checksummed: "28b52ffd240b59000068656c6c6f20776f726c6468691eb2",
  streamed: "28b52ffd005859000068656c6c6f20776f726c64",
  emptyFrame: "28b52ffd2000010000",
};
const frame = (name) => Buffer.from(frames[name], "hex");
attempt("decompress hello", () => zlib.zstdDecompressSync(frame("hello")).toString());
attempt("decompress streamed frame", () => zlib.zstdDecompressSync(frame("streamed")).toString());
attempt("decompress empty frame", () => zlib.zstdDecompressSync(frame("emptyFrame")));
attempt("decompress empty input", () => zlib.zstdDecompressSync(Buffer.alloc(0)));
attempt("first frame only", () =>
  zlib.zstdDecompressSync(Buffer.concat([frame("a"), frame("b")])).toString());
attempt("trailing garbage ignored", () =>
  zlib.zstdDecompressSync(Buffer.concat([frame("a"), Buffer.from("zz")])).toString());
const hello = frame("hello");
for (let i = 0; i <= hello.length; i++) {
  attempt(`truncated to ${i}`, () => zlib.zstdDecompressSync(hello.subarray(0, i)));
}
attempt("checksummed frame", () => zlib.zstdDecompressSync(frame("checksummed")).toString());
{
  const bad = frame("checksummed");
  bad[bad.length - 1] ^= 1;
  attempt("checksum wrong", () => zlib.zstdDecompressSync(bad));
}
attempt("garbage", () => zlib.zstdDecompressSync(Buffer.from("garbage!")));
attempt("window too large", () => zlib.zstdDecompressSync(Buffer.from("28b52ffd00ff", "hex")));
attempt("windowLogMax 10 refuses a 4 KiB window", () =>
  zlib.zstdDecompressSync(
    zlib.zstdCompressSync(Buffer.alloc(5000, "ab"), {
      params: { [C.ZSTD_c_windowLog]: 12, [C.ZSTD_c_contentSizeFlag]: 0 },
    }),
    { params: { [C.ZSTD_d_windowLogMax]: 10 } },
  ));

// A dictionary (an ArrayBufferView; anything else is not passed on).
{
  const dictionary = Buffer.from("hello world hello world");
  const packed = zlib.zstdCompressSync("hello world", { dictionary });
  console.log("with dictionary:", hex(packed));
  attempt("dictionary round trip", () => zlib.zstdDecompressSync(packed, { dictionary }).toString());
  attempt("dictionary missing", () => zlib.zstdDecompressSync(packed));
  attempt("string dictionary unused", () => zlib.zstdCompressSync("hello world", { dictionary: "abc" }));
}

// maxOutputLength, on both sides.
attempt("compress over maxOutputLength", () =>
  zlib.zstdCompressSync("hello hello hello", { maxOutputLength: 5 }));
attempt("decompress over maxOutputLength", () =>
  zlib.zstdDecompressSync(zlib.zstdCompressSync(Buffer.alloc(1000)), { maxOutputLength: 10 }));
attempt("decompress at maxOutputLength", () =>
  zlib.zstdDecompressSync(zlib.zstdCompressSync(Buffer.alloc(1000)), { maxOutputLength: 1000 }).length);

// info: { buffer, engine }.
{
  const r = zlib.zstdCompressSync("xx", { info: true });
  console.log("info:", Object.keys(r).join(","), r.engine.constructor.name, r.engine.bytesWritten, r.engine._closed, hex(r.buffer));
  const d = zlib.zstdDecompressSync(r.buffer, { info: true });
  console.log("info decompress:", d.engine.constructor.name, d.engine.bytesWritten, String(d.buffer));
}

// The callback forms, one at a time, then their promisified form.
await new Promise((resolve) => zlib.zstdCompress("hello hello hello", (err, out) => {
  console.log("callback compress:", err, hex(out));
  resolve();
}));
await new Promise((resolve) => zlib.zstdDecompress(frame("hello"), (err, out) => {
  console.log("callback decompress:", err, String(out));
  resolve();
}));
await new Promise((resolve) => zlib.zstdDecompress(Buffer.from("garbage!"), (err, out) => {
  console.log("callback garbage:", describe(err), out);
  resolve();
}));
await new Promise((resolve) => zlib.zstdDecompress(Buffer.concat([frame("a"), frame("b")]), (err, out) => {
  console.log("callback first frame only:", err, String(out));
  resolve();
}));
await new Promise((resolve) => zlib.zstdCompress("hello", { info: true }, (err, out) => {
  console.log("callback info:", err, Object.keys(out).join(","), out.engine.constructor.name, out.engine.bytesWritten, out.engine._closed);
  resolve();
}));
await new Promise((resolve) => zlib.zstdDecompress(zlib.zstdCompressSync(Buffer.alloc(100)), { maxOutputLength: 50 }, (err) => {
  console.log("callback maxOutputLength:", describe(err));
  resolve();
}));
await new Promise((resolve) => zlib.zstdCompress(null, (err, out) => {
  console.log("callback null input:", err, hex(out));
  resolve();
}));
await new Promise((resolve) => zlib.zstdCompress(text, (err, out) => {
  console.log("callback text:", err, out.length, digest(out));
  resolve();
}));
const compress = promisify(zlib.zstdCompress);
const decompress = promisify(zlib.zstdDecompress);
const packed = await compress("promisified");
console.log("promisify compress:", hex(packed));
console.log("promisify decompress:", String(await decompress(packed)));
try {
  await decompress(Buffer.from("garbage!"));
} catch (e) {
  console.log("promisify garbage:", describe(e));
}
