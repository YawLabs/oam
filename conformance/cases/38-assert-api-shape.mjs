// node:assert API-shape parity: assert.fail() generated message, the Assert
// class (constructable; bare call requires `new`), assert.partialDeepStrictEqual,
// deepStrictEqual comparing Error `cause` and unwrapping boxed primitives,
// and assert.ifError inspecting the offending value.
import assert from "node:assert";

const tag = (label, fn) => {
  try {
    fn();
    console.log(label + ":ok");
  } catch (e) {
    console.log(label + ":throw:" + (e.code || e.name));
  }
};

// assert.fail() with no args -> generated "Failed" message, generatedMessage true.
try {
  assert.fail();
} catch (e) {
  console.log("fail-msg " + JSON.stringify(e.message) + " gen=" + e.generatedMessage);
}

// Assert must require `new`.
tag("Assert-bare", () => assert.Assert());
console.log("Assert-new=" + (new assert.Assert({}) instanceof Object));

// partialDeepStrictEqual: expected is a subset of actual.
tag("partial-ok", () => assert.partialDeepStrictEqual({ a: 1, b: 2 }, { a: 1 }));
tag("partial-bad", () => assert.partialDeepStrictEqual({ a: 1 }, { a: 2 }));

// deepStrictEqual compares Error `cause`.
tag("cause-eq", () =>
  assert.deepStrictEqual(new Error("a", { cause: 1 }), new Error("a", { cause: 1 })));
tag("cause-ne", () =>
  assert.deepStrictEqual(new Error("a", { cause: 1 }), new Error("a", { cause: 2 })));

// Boxed primitives unwrap by value, not by shared keys.
console.log("box-bool-num=" + assert.deepStrictEqual.length >= 0);
tag("box-ne", () => assert.deepStrictEqual(new Number(2), new Number(1)));
tag("box-eq", () => assert.deepStrictEqual(new Number(2), new Number(2)));

// ifError quotes a string value via inspect.
try {
  assert.ifError("boom");
} catch (e) {
  console.log("ifError-msg " + JSON.stringify(e.message));
}

// throws with a missing exception ends the message with a period.
try {
  assert.throws(() => {});
} catch (e) {
  console.log("throws-msg " + JSON.stringify(e.message));
}
