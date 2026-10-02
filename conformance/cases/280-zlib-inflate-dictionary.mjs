// node:zlib's inflaters with the `dictionary` option. oam ignored it: a zlib
// stream whose header asks for a preset dictionary (FDICT) failed
// Z_NEED_DICT "Missing dictionary" even when the right one was passed, and a
// raw stream that copies from its dictionary failed "invalid distance too
// far back". Now the window starts primed with the dictionary, a zlib
// header's DICTID is checked against its Adler-32 ("Bad dictionary"), and
// the option is validated as node's Zlib constructor does.
//
// The compressed inputs are node's own bytes, or built here bit by bit, so
// nothing depends on what oam's deflater writes.
import zlib from "node:zlib";

const hex = (s) => Buffer.from(s, "hex");
function adler32(buf) {
  let a = 1;
  let b = 0;
  for (const x of buf) {
    a = (a + x) % 65521;
    b = (b + a) % 65521;
  }
  return ((b << 16) | a) >>> 0;
}
const be32 = (n) => {
  const b = Buffer.alloc(4);
  b.writeUInt32BE(n);
  return b;
};

// One final fixed-Huffman deflate block (RFC 1951 3.2.6) from literals and
// [length, distance] copies.
function fixedBlock(ops) {
  const bytes = [];
  let acc = 0;
  let n = 0;
  const bits = (value, count) => {
    for (let i = 0; i < count; i++) {
      acc |= ((value >>> i) & 1) << n;
      if (++n === 8) {
        bytes.push(acc);
        acc = 0;
        n = 0;
      }
    }
  };
  const huff = (code, count) => {
    for (let i = count - 1; i >= 0; i--) bits((code >>> i) & 1, 1);
  };
  const sym = (s) => {
    if (s < 144) huff(0x30 + s, 8);
    else if (s < 256) huff(0x190 + s - 144, 9);
    else if (s < 280) huff(s - 256, 7);
    else huff(0xc0 + s - 280, 8);
  };
  const LEN = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
  const LEN_X = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
  const DIST = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
  const last = (table, v) => table.findLastIndex((base) => base <= v);
  bits(1, 1); // BFINAL
  bits(1, 2); // BTYPE 01: fixed codes
  for (const op of ops) {
    if (typeof op === "number") {
      sym(op);
      continue;
    }
    const [length, distance] = op;
    const l = last(LEN, length);
    sym(257 + l);
    bits(length - LEN[l], LEN_X[l]);
    const d = last(DIST, distance);
    huff(d, 5);
    bits(distance - DIST[d], d < 4 ? 0 : (d >> 1) - 1);
  }
  sym(256);
  if (n > 0) bytes.push(acc);
  return Buffer.from(bytes);
}
const zlibWrap = (dictionary, body, out) =>
  Buffer.concat([hex("78bb"), be32(adler32(dictionary)), body, be32(adler32(out))]);

const shape = (e) =>
  `${e.constructor.name} ${e.code} ${e.errno} ${JSON.stringify(e.message)}` +
  ` [${Object.getOwnPropertyNames(e).join(",")}]`;
const show = (label, fn) => {
  try {
    console.log(label, "->", JSON.stringify(fn().toString("latin1")));
  } catch (e) {
    console.log(label, "throws", shape(e));
  }
};

// node v22.22.2: deflateSync / deflateRawSync('hello world hello', { dictionary }).
const dictionary = Buffer.from("hello world dictionary");
const fdict = hex("78bb622008b3cb401205b3013b200691");
const raw = hex("cb401205b301");

console.log("-- sync");
show("inflate, dictionary", () => zlib.inflateSync(fdict, { dictionary }));
show("inflate, none", () => zlib.inflateSync(fdict));
show("inflate, empty", () => zlib.inflateSync(fdict, { dictionary: Buffer.alloc(0) }));
show("inflate, wrong", () => zlib.inflateSync(fdict, { dictionary: Buffer.from("nope") }));
show("unzip, dictionary", () => zlib.unzipSync(fdict, { dictionary }));
show("unzip, wrong", () => zlib.unzipSync(fdict, { dictionary: Buffer.from("nope") }));
show("inflateRaw, dictionary", () => zlib.inflateRawSync(raw, { dictionary }));
show("inflateRaw, none", () => zlib.inflateRawSync(raw));
show("inflateRaw, short wrong", () => zlib.inflateRawSync(raw, { dictionary: Buffer.from("x") }));
show("inflateRaw, long wrong", () => zlib.inflateRawSync(raw, { dictionary: Buffer.alloc(41, "x") }));
// DICTID is checked as soon as it is in; a correct one waits for the body.
show("header only, wrong", () => zlib.inflateSync(fdict.subarray(0, 6), { dictionary: Buffer.from("nope") }));
show("header only, right", () => zlib.inflateSync(fdict.subarray(0, 6), { dictionary }));
show("in DICTID", () => zlib.inflateSync(fdict.subarray(0, 4), { dictionary }));
show("header only, sync flush", () =>
  zlib.inflateSync(fdict.subarray(0, 6), { dictionary, finishFlush: zlib.constants.Z_SYNC_FLUSH }));
show("wrong, sync flush", () =>
  zlib.inflateSync(fdict, { dictionary: Buffer.from("nope"), finishFlush: zlib.constants.Z_SYNC_FLUSH }));
// A dictionary nothing asks for is not used.
const plain = hex("789ccb48cdc9c95728cf2fca4951c800b1013b200691");
show("no FDICT, dictionary", () => zlib.inflateSync(plain, { dictionary }));
show("gunzip, dictionary", () =>
  zlib.gunzipSync(hex("1f8b080000000000000acb48cdc9c95728cf2fca4951c800b1015de5d0a911000000"), { dictionary }));

console.log("-- what counts as a dictionary");
const u8 = new Uint8Array(dictionary);
for (const [name, value] of [
  ["ArrayBuffer", u8.slice().buffer],
  ["DataView", new DataView(u8.slice().buffer)],
  ["Uint16Array", new Uint16Array(u8.slice().buffer)],
  ["subarray", Buffer.concat([Buffer.from("zz"), dictionary]).subarray(2)],
  ["string", "hello world dictionary"],
  ["null", null],
  ["number", 5],
  ["object", {}],
]) {
  show(`inflate, ${name}`, () => zlib.inflateSync(fdict, { dictionary: value }));
}
show("gzip validates it too", () => zlib.gzipSync("x", { dictionary: "x" }));
show("before finishFlush", () => zlib.inflateSync(fdict, { dictionary: 1, finishFlush: 99 }));
show("before maxOutputLength", () => zlib.inflateSync(fdict, { dictionary: 1, maxOutputLength: -1 }));
try {
  zlib.createInflate({ dictionary: "x" });
} catch (e) {
  console.log("createInflate throws", shape(e));
}

console.log("-- the window");
// The dictionary's last 32 KiB is history; a copy from the oldest byte of it
// (distance 32768) needs a dictionary at least that long.
const big = Buffer.alloc(70000);
for (let i = 0; i < big.length; i++) big[i] = 97 + ((i * 7) % 26);
const far = fixedBlock([[5, 32768], 0x21, [4, 32768]]);
const farOut = Buffer.concat([
  big.subarray(big.length - 32768, big.length - 32763),
  Buffer.from("!"),
  big.subarray(big.length - 32762, big.length - 32758),
]);
show("raw, 70000-byte dictionary", () => zlib.inflateRawSync(far, { dictionary: big }));
show("raw, its last 32768 bytes", () => zlib.inflateRawSync(far, { dictionary: big.subarray(big.length - 32768) }));
show("raw, its last 32767 bytes", () => zlib.inflateRawSync(far, { dictionary: big.subarray(big.length - 32767) }));
const bigZ = zlibWrap(big, far, farOut);
console.log("built as expected", zlib.inflateRawSync(far, { dictionary: big }).equals(farOut));
show("zlib, whole dictionary", () => zlib.inflateSync(bigZ, { dictionary: big }));
// DICTID is the whole dictionary's Adler-32, not the window's.
show("zlib, its last 32768 bytes", () => zlib.inflateSync(bigZ, { dictionary: big.subarray(big.length - 32768) }));
// The data's check value does not cover the dictionary.
const badCheck = Buffer.from(bigZ);
badCheck[badCheck.length - 1] ^= 1;
show("zlib, data check", () => zlib.inflateSync(badCheck, { dictionary: big }));

console.log("-- callback and stream");
await new Promise((resolve) =>
  zlib.inflate(fdict, { dictionary }, (err, out) => {
    console.log("inflate cb", err ? shape(err) : out.toString());
    resolve();
  }),
);
await new Promise((resolve) =>
  zlib.inflate(fdict, { dictionary: Buffer.from("nope") }, (err, out) => {
    console.log("inflate cb, wrong", err ? shape(err) : out.toString());
    resolve();
  }),
);
await new Promise((resolve) =>
  zlib.inflateRaw(raw, { dictionary }, (err, out) => {
    console.log("inflateRaw cb", err ? shape(err) : out.toString());
    resolve();
  }),
);
const streamed = async (stream, input) => {
  const parts = [];
  stream.on("data", (c) => parts.push(c));
  const done = new Promise((resolve) => {
    stream.on("end", () => resolve(Buffer.concat(parts).toString()));
    stream.on("error", (e) => resolve("error " + shape(e)));
  });
  for (let i = 0; i < input.length; i += 3) stream.write(input.subarray(i, i + 3));
  stream.end();
  return done;
};
console.log("createInflate", await streamed(zlib.createInflate({ dictionary }), fdict));
console.log("createInflate, none", await streamed(zlib.createInflate(), fdict));
console.log("createInflate, wrong", await streamed(zlib.createInflate({ dictionary: Buffer.from("nope") }), fdict));
console.log("createUnzip", await streamed(zlib.createUnzip({ dictionary }), fdict));
console.log("createInflateRaw", await streamed(zlib.createInflateRaw({ dictionary }), raw));
