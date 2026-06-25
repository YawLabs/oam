// node:string_decoder streaming parity: a StringDecoder must buffer an
// incomplete multi-byte (utf8), 2-byte code unit (utf16le), or 3-byte group
// (base64) across write() calls and flush on end(), matching a whole-buffer
// decode -- and the SAME decoder instance must reset its carry state after
// end() so it can be reused.
import { StringDecoder } from "node:string_decoder";

// Decode a buffer one byte at a time and compare to the whole-buffer decode.
function bytewise(enc, bytes) {
  const buf = Buffer.from(bytes);
  const sd = new StringDecoder(enc);
  let out = "";
  for (let i = 0; i < buf.length; i++) out += sd.write(buf.subarray(i, i + 1));
  out += sd.end();
  return out === buf.toString(enc);
}

// '☃💩€' exercises 3- and 4-byte utf8 sequences.
const utf8Bytes = [...Buffer.from("☃💩€")];
console.log("utf8-bytewise=" + bytewise("utf8", utf8Bytes));
console.log("utf16-bytewise=" + bytewise("utf16le", [...Buffer.from("☃💩€", "utf16le")]));
console.log("base64-bytewise=" + bytewise("base64", utf8Bytes));
console.log("base64url-bytewise=" + bytewise("base64url", utf8Bytes));

// Reuse a single decoder across write/end pairs: an incomplete byte flushed as
// U+FFFD must not leak into the next sequence.
const d = new StringDecoder("utf8");
let s = "";
s += d.write(Buffer.from([0xe2])); // incomplete lead byte
s += d.end(); // flushes one replacement char
s += d.write(Buffer.from([0x61])); // 'a' -- must decode cleanly
s += d.end();
console.log("reuse " + JSON.stringify(s));

// utf16le surrogate split across writes.
const u = new StringDecoder("utf16le");
let g = "";
g += u.write(Buffer.from([0x3d, 0xd8, 0x4d])); // high surrogate + half of low
g += u.write(Buffer.from([0xdc])); // completes the low surrogate
g += u.end();
console.log("surrogate " + JSON.stringify(g));
