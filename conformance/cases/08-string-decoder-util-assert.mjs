// string_decoder boundary safety + util.format + assert semantics.
import { StringDecoder } from "node:string_decoder";
import util from "node:util";
import assert from "node:assert";

const decoder = new StringDecoder("utf8");
const partA = decoder.write(Buffer.from([0x61, 0xe2]));
const partB = decoder.write(Buffer.from([0x82, 0xac]));
console.log(JSON.stringify(partA), JSON.stringify(partB), JSON.stringify(decoder.end()));

console.log(util.format("%s=%d %j", "n", 42, { a: 1 }));
console.log(util.format("%d", Symbol("x")), util.format("%i", "42px"), util.format("%%"));
console.log(util.format("plain", "extra", 7));
console.log(util.types.isDate(new Date(0)), util.types.isPromise(Promise.resolve()), util.isDeepStrictEqual({ a: [1] }, { a: [1] }));

assert.strictEqual(1, 1);
assert.deepStrictEqual({ a: [1, { b: 2 }] }, { a: [1, { b: 2 }] });
let code = "";
try {
  assert.deepStrictEqual([1], [2]);
} catch (e) {
  code = e.code;
}
console.log(code);
let zeroSign = false;
try {
  assert.strictEqual(0, -0);
} catch {
  zeroSign = true;
}
console.log(zeroSign, (assert.strict.equal === assert.strictEqual));
let threwMatch = "";
try {
  assert.throws(() => { throw new TypeError("boom"); }, RangeError);
} catch (e) {
  threwMatch = e.constructor.name;
}
console.log(threwMatch.length > 0);
