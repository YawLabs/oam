// Buffer's string<->bytes paths at their edges, after they were rewritten to
// encode straight into the destination (no temporary) and to decode long
// spans with V8's own decoders. Pins what must not move with that speed-up,
// and the node behaviours oam did not have before it:
// - UTF-8: a surrogate pair is 4 bytes, a lone surrogate U+FFFD (3 bytes);
//   a write never splits a character, whatever the room;
// - invalid UTF-8 decodes to one U+FFFD per maximal invalid subpart, in a
//   short span (decoded in JS) and a long one (decoded natively) alike;
// - base64 reads each code unit's LOW byte (U+013D is '=' and ends the data)
//   and byteLength discounts at most two trailing '=' ("QQ===" is 2);
// - Buffer.from(string) places its bytes in the pool as node's
//   fromStringFast does: 4 bytes per code unit reserved, the own-buffer
//   cut-off on code units, and a shortfall kept as a shorter view;
// - Buffer.concat allocates from the pool and zero-fills past the list;
// - toString over an empty range is "" even for an unknown encoding;
// - TextEncoder#encodeInto validates its arguments and reports `read`
//   exactly. Measured on node v22.22.2.
import { Buffer } from "node:buffer";

const hex = (b) => Buffer.prototype.toString.call(b, "hex");
const show = (label, f) => {
  try {
    console.log(label, JSON.stringify(f()));
  } catch (e) {
    console.log(label, "THROW", e.name, e.code, e.message);
  }
};

const ENCODINGS = ["utf8", "latin1", "ascii", "hex", "base64", "base64url", "ucs2"];
const STRINGS = [
  "", "a", "é", "€", "\u{1F600}", "\ud800", "\udc00", "a\ud800", "\ud800a",
  "\ude00\ud83d", "\ud83d\u{1F600}", "éé\ud83d", "xĀy", "\u0000\u007f\u0080",
  "QQ==", "QQ===", "Q=Q=", "=", "Q Q\n==", "a-_b", "QUJDĽQUJD", "őUJD", "QUJDīQQ",
  "zz", "abc", "0g", "١İ",
];

// from / byteLength / every partial write length, every encoding.
for (const enc of ENCODINGS) {
  for (const s of STRINGS) {
    const writes = [];
    for (let room = 0; room <= 6; room++) {
      const t = Buffer.alloc(8, 0xaa);
      writes.push(t.write(s, 1, room, enc) + ":" + hex(t));
    }
    console.log(enc, JSON.stringify(s), hex(Buffer.from(s, enc)), Buffer.byteLength(s, enc), writes.join(" "));
  }
}

// UTF-8 byte counts of long strings (counted natively past 128 code units).
show("byteLength long", () => [
  Buffer.byteLength("\ud800a\u{1F600}é".repeat(100)),
  Buffer.byteLength("\udc00".repeat(129)),
  Buffer.byteLength("\ud83d".repeat(128) + "\ude00"),
  Buffer.byteLength("\ude00\ud83d".repeat(70)),
  new TextEncoder().encode("\ud800€".repeat(90)).length,
]);

// Decodes, short (<= 32 bytes) and long, valid and not.
const SEQUENCES = [
  "c3", "c3a9c3", "e282", "eda080", "edbfbf", "f09f98", "f4908080", "f0808080", "c080", "e08080",
  "f8", "80", "bf80", "efbbbf41", "e282e282ac", "f09f41", "00017f80ff", "e0a0", "f09f9880f09f98",
];
for (const enc of ["utf8", "latin1", "ascii", "ucs2"]) {
  for (const h of SEQUENCES) {
    const b = Buffer.from(h, "hex");
    const long = Buffer.concat([Buffer.alloc(40, 0x61), b, Buffer.alloc(40, 0x62)]);
    const decodedLong = long.toString(enc);
    console.log("decode", enc, h, JSON.stringify(b.toString(enc)), JSON.stringify(b.toString(enc, 1)),
      decodedLong.length, JSON.stringify(decodedLong.slice(38, 46)));
  }
}
const mixed = Buffer.from("café \u{1F600} \ud800 ".repeat(20));
for (let i = 0; i < 40; i += 3) {
  show(`slice ${i}`, () => [mixed.toString("utf8", i, i + 50).length, mixed.utf8Slice(i, i + 70), mixed.ucs2Slice(i, i + 9)]);
}
show("empty-range unknown encoding", () => [Buffer.from("ab").toString("bogus", 2), Buffer.alloc(0).toString("bogus")]);
show("unknown encoding", () => Buffer.from("ab").toString("bogus"));

// Pool placement: each sequence starts on an exactly full pool.
function fillPool() {
  for (let i = 0; i < 4; i++) if (Buffer.allocUnsafe(4095).byteOffset === 4096) return;
  throw new Error("pool never filled");
}
function placement(label, make, n) {
  fillPool();
  const out = [];
  let pool = null;
  for (let i = 0; i < n; i++) {
    const b = make(i);
    const tag = b.buffer === pool ? "=" : b.buffer.byteLength === Buffer.poolSize ? "^" : "!";
    if (b.buffer.byteLength === Buffer.poolSize) pool = b.buffer;
    out.push(`${b.length}${tag}${b.byteOffset}/${b.buffer.byteLength}`);
  }
  console.log("pool", label, out.join(" "));
}
placement("utf8 1000", () => Buffer.from("a".repeat(1000)), 7);
placement("utf8 1023", () => Buffer.from("a".repeat(1023)), 6);
placement("utf8 1500", () => Buffer.from("a".repeat(1500)), 7);
placement("utf8 e-acute", () => Buffer.from("é".repeat(1100)), 5);
placement("utf8 lone", () => Buffer.from("\ud800".repeat(700)), 5);
// >= poolSize/2 bytes from < poolSize/2 code units, with room in the pool.
placement("utf8 own", (i) => (i === 0 ? Buffer.from("x") : Buffer.from("€".repeat(1400 + i))), 4);
show("utf8 own bytes", () => hex(Buffer.from("€\u{1F600}".repeat(700)).subarray(4890)));
placement("latin1", () => Buffer.from("é".repeat(900), "latin1"), 7);
placement("ucs2", () => Buffer.from("ab".repeat(300), "ucs2"), 7);
placement("hex 5000 chars", () => Buffer.from("ab".repeat(2500), "hex"), 2);
placement("hex bad tail", () => Buffer.from("abzz".repeat(300), "hex"), 4);
placement("base64 ws", () => Buffer.from("QUJD \n".repeat(200), "base64"), 4);
placement("mixed", (i) => [
  Buffer.from("x".repeat(10)), Buffer.from("\ud800"), Buffer.from("zzz", "hex"),
  Buffer.allocUnsafe(1), Buffer.from(""), Buffer.from("abc", "ucs2"),
][i], 6);
placement("concat", (i) => Buffer.concat([Buffer.from("ab"), new Uint8Array([1, 2])], i * 3), 4);
show("concat zero tail", () => hex(Buffer.concat([Buffer.from("ab")], 6)));
show("own shortfall", () => {
  const h = Buffer.from("zz".repeat(3000), "hex");
  const b = Buffer.from("QUJD \n".repeat(1000), "base64");
  return [h.length, h.buffer.byteLength, b.length, b.buffer.byteLength];
});

// TextEncoder.
const te = new TextEncoder();
show("encodeInto object", () => te.encodeInto("a", {}));
show("encodeInto Uint16Array", () => te.encodeInto("a", new Uint16Array(2)));
show("encodeInto number", () => te.encodeInto(5, new Uint8Array(2)));
show("encodeInto pair cut", () => te.encodeInto("\u{1F600}a", new Uint8Array(3)));
show("encodeInto lone", () => te.encodeInto("a\ud800b", new Uint8Array(4)));
show("encode", () => Array.from(te.encode("\ud800x\u{1F600}\udc00")));
