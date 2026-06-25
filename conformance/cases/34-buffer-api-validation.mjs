// Buffer static/instance API validation + inspect/INSPECT_MAX_BYTES + parent.
// Node throws ERR_INVALID_ARG_TYPE (with the rich "Received ..." suffix) for
// bad equals/compare/concat/from/byteLength args, ERR_UNKNOWN_ENCODING for bad
// encodings, and validates isUtf8/isAscii input. Guards classes 2/3/5/6/7.
import buffer from "node:buffer";
import util from "node:util";

const cap = (fn) => {
  try {
    fn();
    return "NO-THROW";
  } catch (e) {
    return `${e.code}|${e.name}|${e.message}`;
  }
};
const b = Buffer.from("abc");

// Instance + static argument-type validation (full messages).
console.log(cap(() => b.equals("x")));
console.log(cap(() => b.compare("x")));
console.log(cap(() => Buffer.compare("x", b)));
console.log(cap(() => Buffer.concat(5)));
console.log(cap(() => Buffer.concat([Buffer.from("a"), 3])));
console.log(cap(() => Buffer.from(5)));
console.log(cap(() => Buffer.byteLength(5)));
console.log(cap(() => b.toString("not-an-encoding")));
console.log(cap(() => b.indexOf({})));

// from() coerces String objects / Symbol.toPrimitive to their string value.
class P {
  [Symbol.toPrimitive]() {
    return "test";
  }
}
console.log(Buffer.from(new String("test")).toString());
console.log(Buffer.from(new P()).toString());
console.log(Buffer.concat([], 100).length);

// isUtf8 / isAscii validate input type.
console.log(cap(() => buffer.isAscii(5)));
console.log(cap(() => buffer.isUtf8(5)));
console.log(buffer.isAscii(Buffer.from("abc")), buffer.isUtf8(Buffer.from("é")));

// INSPECT_MAX_BYTES drives inspect truncation and validates on assignment.
buffer.INSPECT_MAX_BYTES = 2;
console.log(util.inspect(Buffer.from("1234")));
buffer.INSPECT_MAX_BYTES = 50;
console.log(util.inspect(Buffer.from("x".repeat(51))));
console.log(cap(() => (buffer.INSPECT_MAX_BYTES = -1)));
console.log(cap(() => (buffer.INSPECT_MAX_BYTES = "nope")));

// parent / offset aliases.
const ab = new ArrayBuffer(0);
console.log(Buffer.from(ab).parent === ab, Buffer.alloc(4).offset === 0);
