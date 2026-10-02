// How a coded error's stack header is rendered and what its own keys are,
// against node v22.22.2.
//
// node's defaultPrepareStackTrace (lib/internal/errors.js) writes the header
// on the stack's FIRST read: `${name} [${code}]: ${message}` for an error
// carrying [kIsNodeError] (one built by its internal errors), and the
// intrinsic Error.prototype.toString for any other -- never the error's own
// toString. oam rendered every header through `${err}`, so AssertionError
// (whose name carries the code while its stack is rendered) showed the code
// twice, a class overriding toString leaked it into the stack, and a code
// changed before the first read did not show.
import assert from "node:assert";
import fs from "node:fs";

const head = (e) => JSON.stringify(String(e.stack).split("\n")[0]);
const caught = (fn) => {
  try {
    fn();
  } catch (e) {
    return e;
  }
  return null;
};

console.log("== stack header: node's defaultPrepareStackTrace");
console.log("assert.strictEqual", head(caught(() => assert.strictEqual(1, 2))));
console.log("assert.ok", head(caught(() => assert.ok(false))));
console.log("new AssertionError", head(new assert.AssertionError({ message: "m" })));
class OwnToString extends Error {
  toString() {
    return "CUSTOM";
  }
}
console.log("own toString", head(new OwnToString("m")), String(new OwnToString("m")));
{
  const e = caught(() => fs.readFileSync(1.5));
  e.code = "ERR_X";
  console.log("code changed before the first read", head(e));
}
{
  const e = caught(() => fs.readFileSync(1.5));
  void e.stack;
  e.code = "ERR_X";
  console.log("code changed after the first read", head(e));
}
{
  const e = new TypeError("plain");
  e.code = "ERR_PLAIN";
  console.log("a plain error with a code", head(e));
}
