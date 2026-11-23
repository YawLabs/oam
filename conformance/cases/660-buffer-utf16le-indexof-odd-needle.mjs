// buf.indexOf / buf.lastIndexOf with a ucs2/utf16le encoding, pinned against
// node on the same host.
//
// The search runs over whole code units. node does not refuse an odd-length
// Buffer needle: it drops the needle's trailing byte and searches for the
// rest, so `-1` comes back only for a needle under 2 bytes. oam answered -1
// for every odd-length needle. The fit checks, the rounding of a mid-unit
// offset (down for a Buffer needle, up for a string one), a string needle's
// offsets counting from the haystack's even length, and an empty needle's
// answer are pinned too: by the explicit lines first, then by the sweep at
// the end, printed as a row count and a digest of every answer.
import { createHash } from "node:crypto";

const hay = Buffer.from("abcabc", "utf16le");
const odd = (bytes) => Buffer.from(bytes);

// "b" plus one stray byte: found as "b".
console.log("odd needle", hay.indexOf(odd([0x62, 0, 0x63]), 0, "utf16le"), hay.lastIndexOf(odd([0x62, 0, 0x63]), undefined, "utf16le"));
// Under 2 bytes: never found.
console.log("one byte", hay.indexOf(odd([0x62]), 0, "utf16le"), hay.lastIndexOf(odd([0x62]), undefined, "utf16le"));
// "bc" plus one stray byte, as a Uint8Array and through the aliases.
const bc = new Uint8Array([0x62, 0, 0x63, 0, 0x99]);
console.log("aliases", hay.indexOf(bc, 0, "ucs2"), hay.lastIndexOf(bc, -1, "utf-16le"), hay.includes(bc, "ucs-2"));
// A mid-unit offset: down for a Buffer needle, up for a string needle.
console.log("offset 3", hay.indexOf(odd([0x62, 0, 0x63]), 3, "utf16le"), hay.indexOf("b", 3, "utf16le"));
// The needle's full byte length must fit after the offset.
const aaaa = Buffer.from("aaaa", "utf16le");
console.log("fit", aaaa.indexOf(odd([0x61, 0, 0x61]), 5, "utf16le"), aaaa.indexOf(odd([0x61, 0, 0x61]), 6, "utf16le"));
// An odd-length haystack: its last byte is not a code unit.
const tail = Buffer.from([0x61, 0, 0x62, 0, 0x63]);
console.log("odd haystack", tail.indexOf(odd([0x63, 0, 1]), 0, "utf16le"), tail.lastIndexOf(odd([0x62, 0, 1]), undefined, "utf16le"), tail.indexOf(odd([0x61, 0, 0x62, 0, 0x63]), 0, "utf16le"));
// Empty needles, and one before the start of the buffer.
console.log("empty", tail.lastIndexOf("", undefined, "utf16le"), tail.lastIndexOf(Buffer.alloc(0), -1, "utf16le"), tail.lastIndexOf("", -1, "utf16le"), tail.lastIndexOf(Buffer.alloc(0), -Infinity), tail.lastIndexOf("", -9, "latin1"));

// The sweep: every haystack and needle over {0x00, 0x61} up to 6 and 5
// bytes (string needles up to 2 characters), across offsets, encodings and
// needle kinds.
const offsets = [undefined, NaN, Infinity, -Infinity, 1.5, -1.5, 1e10, -1e10, null];
for (let k = -7; k <= 7; k++) offsets.push(k);
const encodings = ["utf16le", "latin1"];
const bytesOf = (n, bits) => Array.from({ length: n }, (_, i) => ((bits >> i) & 1 ? 0x61 : 0));
const needles = [];
for (let n = 0; n <= 5; n++) {
  for (let bits = 0; bits < 1 << n; bits++) {
    needles.push(Buffer.from(bytesOf(n, bits)));
    if (n === 3) needles.push(new Uint8Array(bytesOf(n, bits)));
  }
}
for (let n = 0; n <= 2; n++) {
  for (let bits = 0; bits < 1 << n; bits++) {
    needles.push(Array.from({ length: n }, (_, i) => ((bits >> i) & 1 ? "a" : "\0")).join(""));
  }
}
const hash = createHash("sha256");
let rows = 0;
for (let n = 0; n <= 6; n++) {
  for (let bits = 0; bits < 1 << n; bits++) {
    const haystack = Buffer.from(bytesOf(n, bits));
    for (const needle of needles) {
      for (const offset of offsets) {
        for (const encoding of encodings) {
          hash.update(`${haystack.indexOf(needle, offset, encoding)},${haystack.lastIndexOf(needle, offset, encoding)};`);
          rows++;
        }
      }
    }
  }
}
console.log("sweep", rows, hash.digest("hex"));
