// node's string_decoder.StringDecoder is a FUNCTION constructor, not a
// class, and packages inherit from it the ES5 way. iconv-lite 0.4.24 --
// raw-body 2 -> body-parser 1 -> express 4, and needle, and superagent --
// is exactly:
//
//   function InternalDecoder(options, codec) { StringDecoder.call(this, codec.enc); }
//   InternalDecoder.prototype = StringDecoder.prototype;
//
// A class throws "Class constructor StringDecoder cannot be invoked without
// 'new'" there, so every express 4 express.json() / express.urlencoded()
// request answered a 500.
import { StringDecoder } from "node:string_decoder";
import { Buffer } from "node:buffer";

// iconv-lite 0.4.24's encodings/internal.js, verbatim in shape.
function InternalDecoder(options, codec) {
  StringDecoder.call(this, codec.enc);
}
InternalDecoder.prototype = StringDecoder.prototype;

for (const enc of ["utf8", "utf16le", "base64", "latin1", "ascii", "hex"]) {
  try {
    const d = new InternalDecoder({}, { enc });
    const bytes = Buffer.from("hé€llo", "utf8");
    // Feed it in two pieces so an incomplete multi-byte sequence has to be
    // carried across the boundary by the .call-built instance.
    const text = d.write(bytes.subarray(0, 4)) + d.write(bytes.subarray(4)) + d.end();
    console.log(`${enc} ${JSON.stringify(text)} encoding=${d.encoding}`);
  } catch (e) {
    console.log(`${enc} ERR ${e.message}`);
  }
}

// A lone lead byte still flushes as U+FFFD through the .call-built decoder.
const d = new InternalDecoder({}, { enc: "utf8" });
console.log("partial", JSON.stringify(d.write(Buffer.from([0xe2, 0x82])) + d.end()));

// The constructor's shape, as node's plain function has it.
const desc = Object.getOwnPropertyDescriptor(StringDecoder, "prototype");
console.log(
  `prototype w=${desc.writable} e=${desc.enumerable} c=${desc.configurable}`,
  `name=${StringDecoder.name} length=${StringDecoder.length}`,
  `constructor=${StringDecoder.prototype.constructor === StringDecoder}`,
);
for (const key of ["write", "end", "text"]) {
  const d2 = Object.getOwnPropertyDescriptor(StringDecoder.prototype, key);
  console.log(`prototype.${key} fn=${typeof d2.value} e=${d2.enumerable} w=${d2.writable}`);
}
// Called with no receiver at all, as a plain function in strict mode.
try {
  StringDecoder("utf8");
  console.log("no receiver: no throw");
} catch (e) {
  console.log(`no receiver: ${e.constructor.name} ${e.message}`);
}
// new still works, and an unknown encoding is still refused.
const n = new StringDecoder("utf8");
console.log("new", JSON.stringify(n.write(Buffer.from("ok")) + n.end()));
try {
  new StringDecoder("no-such-encoding");
} catch (e) {
  console.log("unknown encoding", e.code);
}
