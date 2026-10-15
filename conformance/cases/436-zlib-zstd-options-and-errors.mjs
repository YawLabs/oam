// node:zlib's zstd options and the errors for bad ones (node v22.22.2). The
// Zstd constructor checks `params` first: each key must be a number from 0 to
// the highest ZSTD_c_* (402) for a compressor or ZSTD_d_* (100) for a
// decompressor, named once ('0x64' and 100 are both 100), with a number or
// boolean value -- RangeError ERR_ZSTD_INVALID_PARAM or ERR_INVALID_ARG_TYPE.
// Values go to libzstd through a Uint32Array (so -1 means "not set" and a
// fraction truncates); a key or value libzstd refuses is a plain Error
// ERR_ZLIB_INITIALIZATION_FAILED "Setting parameter failed", while an
// out-of-range level is clamped and passes. A negative number
// `pledgedSrcSize` is a TypeError. Only then does ZlibBase check chunkSize,
// flush and finishFlush (ZSTD_e_continue..ZSTD_e_end) and maxOutputLength.
// The classes are ES classes; the one-shot forms check their input after the
// options and the callback forms their callback.
//
// Regression guard: oam had no zstd, so every one of these calls threw a
// TypeError for a missing function.
//
// Prints error class, code and message, and never a compressed byte.
import zlib from "node:zlib";

const C = zlib.constants;
const describe = (e) =>
  `${e.constructor.name} ${e.code} ${JSON.stringify(e.message)} ` +
  `stack=${JSON.stringify(e.stack.split("\n")[0])}`;
const attempt = (label, fn) => {
  try {
    const out = fn();
    console.log(`${label}: ok ${Buffer.isBuffer(out) ? `buffer ${out.length}` : typeof out}`);
  } catch (e) {
    console.log(`${label}: ${describe(e)}`);
  }
};
const compress = (opts) => () => zlib.zstdCompressSync("hello hello hello", opts);
const decompress = (opts) => () =>
  zlib.zstdDecompressSync(zlib.zstdCompressSync("hello"), opts);

// params keys.
for (const key of ["-1", "0", "5", "99", "100", "130", "140", "400", "402", "403", "9999", "0x64", "1e2", "abc", "1.5"]) {
  attempt(`compress key ${key}`, compress({ params: { [key]: 1 } }));
}
for (const key of ["-1", "0", "5", "99", "100", "101", "0x64"]) {
  attempt(`decompress key ${key}`, decompress({ params: { [key]: 10 } }));
}
attempt("key named twice", compress({ params: { 100: 3, "0x64": 5 } }));
attempt("key set to -1 then named again", compress({ params: { 100: -1, "0x64": 5 } }));

// params values.
const P = C.ZSTD_c_compressionLevel;
for (const [label, value] of [
  ["string", "3"], ["null", null], ["undefined", undefined], ["object", {}], ["bigint", 3n],
  ["true", true], ["false", false], ["NaN", NaN], ["Infinity", Infinity], ["1.7", 1.7],
  ["2**32+1", 2 ** 32 + 1], ["-200000", -200000], ["100", 100], ["-1", -1],
]) {
  attempt(`level value ${label}`, compress({ params: { [P]: value } }));
}
for (const [label, key, value] of [
  ["windowLog 9", C.ZSTD_c_windowLog, 9],
  ["windowLog 99", C.ZSTD_c_windowLog, 99],
  ["windowLog 0", C.ZSTD_c_windowLog, 0],
  ["windowLog -5", C.ZSTD_c_windowLog, -5],
  ["hashLog 99", C.ZSTD_c_hashLog, 99],
  ["chainLog 99", C.ZSTD_c_chainLog, 99],
  ["searchLog 99", C.ZSTD_c_searchLog, 99],
  ["minMatch 99", C.ZSTD_c_minMatch, 99],
  ["targetLength 2**20", C.ZSTD_c_targetLength, 2 ** 20],
  ["strategy 0", C.ZSTD_c_strategy, 0],
  ["strategy 9", C.ZSTD_c_strategy, 9],
  ["strategy 10", C.ZSTD_c_strategy, 10],
  ["ldm 1", C.ZSTD_c_enableLongDistanceMatching, 1],
  ["ldm 3", C.ZSTD_c_enableLongDistanceMatching, 3],
  ["ldm true", C.ZSTD_c_enableLongDistanceMatching, true],
  ["ldmHashLog 99", C.ZSTD_c_ldmHashLog, 99],
  ["ldmMinMatch 99", C.ZSTD_c_ldmMinMatch, 99],
  ["ldmBucketSizeLog 99", C.ZSTD_c_ldmBucketSizeLog, 99],
  ["ldmHashRateLog 99", C.ZSTD_c_ldmHashRateLog, 99],
  ["contentSizeFlag 7", C.ZSTD_c_contentSizeFlag, 7],
  ["dictIDFlag 0", C.ZSTD_c_dictIDFlag, 0],
  ["nbWorkers 2", C.ZSTD_c_nbWorkers, 2],
  ["nbWorkers 9999", C.ZSTD_c_nbWorkers, 9999],
  ["jobSize 1", C.ZSTD_c_jobSize, 1],
  ["overlapLog 10", C.ZSTD_c_overlapLog, 10],
  ["overlapLog 9", C.ZSTD_c_overlapLog, 9],
]) {
  attempt(`compress ${label}`, compress({ params: { [key]: value } }));
}
for (const value of [0, 9, 10, 31, 32]) {
  attempt(`windowLogMax ${value}`, decompress({ params: { [C.ZSTD_d_windowLogMax]: value } }));
}
// params of other shapes: their own keys are read.
attempt("params null", compress({ params: null }));
attempt("params number", compress({ params: 5 }));
attempt("params string", compress({ params: "ab" }));
attempt("params array", compress({ params: [5] }));

// pledgedSrcSize: a number is read as an integer; a negative one is refused.
for (const [label, value] of [
  ["-1", -1], ["-0.5", -0.5], ["-Infinity", -Infinity], ["Infinity", Infinity], ["NaN", NaN],
  ["2**64", 2 ** 64], ["'5'", "5"], ["null", null], ["true", true], ["5", 5],
]) {
  attempt(`pledgedSrcSize ${label}`, compress({ pledgedSrcSize: value }));
}
attempt("params error before pledgedSrcSize", compress({ params: { 9999: 1 }, pledgedSrcSize: -1 }));
attempt("pledgedSrcSize before flush", compress({ pledgedSrcSize: -1, flush: 9 }));
attempt("parameter failure before flush", compress({ params: { 5: 1 }, flush: 9 }));

// ZlibBase's options, with zstd's flush bounds.
for (const [label, opts] of [
  ["flush 2", { flush: 2 }], ["flush 3", { flush: 3 }], ["flush -1", { flush: -1 }],
  ["flush '1'", { flush: "1" }], ["finishFlush 3", { finishFlush: 3 }],
  ["chunkSize 63", { chunkSize: 63 }], ["chunkSize 64", { chunkSize: 64 }],
  ["chunkSize Infinity", { chunkSize: Infinity }],
  ["maxOutputLength 0", { maxOutputLength: 0 }], ["maxOutputLength 2**53", { maxOutputLength: 2 ** 53 }],
  ["level (zlib's) ignored", { level: 99 }], ["windowBits ignored", { windowBits: 99 }],
  ["strategy ignored", { strategy: 99 }], ["dictionary of a number unused", { dictionary: 5 }],
  ["highWaterMark -1", { highWaterMark: -1 }], ["signal not a signal", { signal: 5 }],
]) {
  attempt(`compress ${label}`, compress(opts));
}

// Inputs, after the options.
for (const [label, input] of [
  ["number", 5], ["null", null], ["undefined", undefined], ["object", {}], ["array", [1, 2]],
]) {
  attempt(`compress input ${label}`, () => zlib.zstdCompressSync(input));
  attempt(`decompress input ${label}`, () => zlib.zstdDecompressSync(input));
}
attempt("options before input", () => zlib.zstdCompressSync(5, { flush: 9 }));

// The callback forms: options and callback checked synchronously.
attempt("callback form, callback not a function", () => zlib.zstdCompress("x", {}, 5));
attempt("callback form, no callback", () => zlib.zstdCompress("x"));
attempt("callback form, bad params", () => zlib.zstdCompress("x", { params: { 9999: 1 } }, () => {}));
attempt("callback form, bad input", () => zlib.zstdCompress(5, () => {}));

// The classes.
attempt("ZstdCompress without new", () => zlib.ZstdCompress());
attempt("ZstdDecompress without new", () => zlib.ZstdDecompress({}));
attempt("new ZstdCompress bad params", () => new zlib.ZstdCompress({ params: { 160: 3 } }));
attempt("createZstdDecompress bad params", () => zlib.createZstdDecompress({ params: { 100: 9 } }));
{
  const s = new zlib.ZstdCompress();
  let proto = Object.getPrototypeOf(s);
  const chain = [];
  for (let i = 0; i < 4; i++) {
    chain.push(proto.constructor.name);
    proto = Object.getPrototypeOf(proto);
  }
  console.log("prototype chain:", chain.join(" > "));
  console.log("own on ZstdCompress.prototype:", Object.getOwnPropertyNames(zlib.ZstdCompress.prototype).join(","));
  console.log("instanceof Transform:", s instanceof (await import("node:stream")).Transform);
  console.log(
    "fields:", s._chunkSize, s._defaultFlushFlag, s._finishFlushFlag, s._defaultFullFlushFlag,
    s._maxOutputLength, s.bytesWritten, s._closed, s._handle.constructor.name,
  );
  console.log("methods:", ["params", "flush", "reset", "close", "_processChunk"].map((m) => `${m}=${typeof s[m]}`).join(" "));
  console.log("static parent:", Object.getPrototypeOf(zlib.ZstdCompress).name);
  s.close();
  const d = zlib.createZstdDecompress({ chunkSize: 128, maxOutputLength: 10, flush: 1, finishFlush: 0 });
  console.log(
    "decompress fields:", d._chunkSize, d._defaultFlushFlag, d._finishFlushFlag,
    d._maxOutputLength, d._handle.constructor.name,
  );
  d.close();
}
