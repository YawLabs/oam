// Buffer#toString's receiver dispatch and its decode across span lengths.
// - node's toString calls the receiver's own `<enc>Slice` method
//   (`this.utf8Slice` with no encoding, `buf.<enc>Slice` otherwise), so a
//   plain Uint8Array, a Uint16Array, a DataView or a plain object as the
//   receiver is a TypeError naming that method -- except an empty range
//   with an encoding given, which is "" before any lookup; an unknown
//   encoding is ERR_UNKNOWN_ENCODING first; an overriding method is called.
//   oam once decoded such receivers, element-wise for a short span and
//   byte-wise for a long one, so the two disagreed with each other too.
// - every decodable kind gives the same string at every span length, on
//   both sides of the JS/native crossover, at odd offsets, and over a
//   SharedArrayBuffer (decoded without the native op). Measured on node
//   v22.22.2.
import { Buffer } from "node:buffer";

const show = (label, f) => {
  try {
    console.log(label, JSON.stringify(f()));
  } catch (e) {
    console.log(label, "THROW", e.name, e.code, e.message);
  }
};

const receivers = [
  ["u8-5", () => new Uint8Array([104, 105, 106, 107, 108])],
  ["u8-0", () => new Uint8Array(0)],
  ["u8-80", () => new Uint8Array(80).fill(65)],
  ["u16-16", () => new Uint16Array(16).fill(0x4142)],
  ["u16-40", () => new Uint16Array(40).fill(0x4142)],
  ["dataview", () => new DataView(new ArrayBuffer(4))],
  ["object", () => ({ length: 3 })],
];
const argsets = [
  [], ["utf8"], ["latin1"], ["ascii"], ["hex"], ["base64"], ["base64url"],
  ["ucs2", 0, 2], [undefined], [undefined, 1], ["bogus"], ["utf8", 0, 0],
];
for (const [name, make] of receivers) {
  for (const args of argsets) {
    show(`${name} ${JSON.stringify(args)}`, () => Buffer.prototype.toString.apply(make(), args));
  }
}

// A receiver that brings its own method gets it called, with the range.
show("override", () => {
  const b = Buffer.from("hello world");
  b.utf8Slice = (s, e) => `utf8Slice(${s},${e})`;
  b.latin1Slice = (s, e) => `latin1Slice(${s},${e})`;
  return [b.toString(), b.toString(undefined, 2), b.toString("latin1", 1, 4), b.toString("hex")];
});
show("borrowed", () => {
  const o = { length: 2, utf8Slice: Buffer.prototype.utf8Slice };
  return Buffer.prototype.toString.call(o);
});
show("Buffer.prototype", () => Buffer.prototype.toString.call(Buffer.prototype));

// Bytes that exercise every decoder: ASCII, 2/3/4-byte UTF-8, invalid and
// truncated sequences, high latin1 bytes.
const pattern = Buffer.from([
  0x61, 0xc3, 0xa9, 0xe2, 0x82, 0xac, 0xf0, 0x9f, 0x98, 0x80, 0xff, 0xc3,
  0x41, 0xed, 0xa0, 0x80, 0xe2, 0x82, 0x7f, 0x80, 0xf4, 0x90, 0x80, 0x80,
]);
const big = Buffer.alloc(400);
for (let i = 0; i < big.length; i++) big[i] = pattern[i % pattern.length];
// FNV-1a over the UTF-16 code units: the whole string in eight hex digits.
const fnv = (s) => {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 0x01000193) >>> 0;
  return h.toString(16).padStart(8, "0");
};
const lengths = [0, 1, 2, 31, 32, 33, 47, 48, 49, 63, 64, 65, 95, 96, 97, 127, 128, 129, 255, 256, 257, 399];
const sab = new SharedArrayBuffer(big.length);
const shared = Buffer.from(sab);
big.copy(shared);
for (const enc of ["utf8", "latin1", "ascii", "ucs2"]) {
  const row = [];
  for (const n of lengths) {
    for (const at of [0, 1]) {
      if (at + n > big.length) continue;
      const s = big.toString(enc, at, at + n);
      const sub = big.subarray(at, at + n).toString(enc);
      const sh = shared.toString(enc, at, at + n);
      row.push(`${n}@${at}:${s.length}:${sub === s}:${sh === s}:${fnv(s)}`);
    }
  }
  console.log(enc, row.join(" "));
}
// The *Slice natives on a SharedArrayBuffer-backed buffer.
show("sab slices", () => [
  shared.utf8Slice(0, 200).length,
  shared.latin1Slice(1, 150).length,
  shared.asciiSlice(0, 300).length,
  shared.ucs2Slice(1, 260).length,
]);
