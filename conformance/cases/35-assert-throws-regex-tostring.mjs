// assert.throws / assert.rejects error-matching parity (FIX 1).
// Node matches a RegExp `expected` against String(err) (err.toString(), which
// for coded errors renders "TypeError [ERR_X]: msg"), NOT the bare .message.
// Validation objects compare per-key: a RegExp value is `.test`ed against
// String(error[key]); any other value (string message/name/code) is
// deepStrictEqual'd -- so a string `message` compares by EQUALITY, a RegExp
// `message` by `.test`. Guards the assert factory's checkExpected().
import assert from "node:assert";

const tag = (label, fn) => {
  try {
    fn();
    console.log(label + ":ok");
  } catch {
    console.log(label + ":throw");
  }
};

// (a) RegExp tested against the rendered toString of a coded error.
const coded = new TypeError("value is out of range");
coded.code = "ERR_OUT_OF_RANGE";
Object.defineProperty(coded, "toString", {
  value() {
    return "TypeError [ERR_OUT_OF_RANGE]: " + this.message;
  },
});
tag("regex-toString", () => assert.throws(() => { throw coded; }, /ERR_OUT_OF_RANGE/));

// (a) RegExp against a plain message.
tag("regex-msg", () => assert.throws(() => { throw new TypeError("x"); }, /x/));

// (b) Validation object: string `message` is EQUALITY -- a substring must NOT match.
tag("obj-msg-eq-exact", () =>
  assert.throws(() => { throw new TypeError("exact msg"); }, { name: "TypeError", message: "exact msg" }));
tag("obj-msg-eq-substr", () =>
  assert.throws(() => { throw new TypeError("partial here"); }, { message: "partial" }));

// (b) Validation object: RegExp `message` is `.test`.
tag("obj-msg-regex", () =>
  assert.throws(() => { throw new TypeError("partial here"); }, { message: /partial/ }));

// (b) Validation object: code + RegExp message together.
const ranged = new RangeError("oops out of range");
ranged.code = "ERR_OUT_OF_RANGE";
tag("obj-code-regex", () =>
  assert.throws(() => { throw ranged; }, { code: "ERR_OUT_OF_RANGE", message: /oops/ }));

// (b) name as RegExp is tested too.
tag("obj-name-regex", () =>
  assert.throws(() => { throw new TypeError("z"); }, { name: /Type/ }));

// (c) Error-constructor `expected` at depth >= 2: a class check at any depth,
// not a validation fn. A non-matching throw reports the mismatch cleanly --
// must NOT throw "Class constructor cannot be invoked without 'new'".
class A extends Error {}
class B extends A {}
tag("ctor-depth2-match", () => assert.throws(() => { throw new B("hi"); }, B));
tag("ctor-depth2-nomatch", () => assert.throws(() => { throw new A("hi"); }, B));

// async parity: assert.rejects shares the same matching logic.
await assert.rejects(async () => { throw coded; }, /ERR_OUT_OF_RANGE/);
console.log("rejects-regex-toString:ok");
