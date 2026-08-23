// util.getSystemErrorName / util.getSystemErrorMessage validate their argument
// before touching the errno table: a non-number is ERR_INVALID_ARG_TYPE
// (TypeError), and a number that is not a NEGATIVE INTEGER -- 0, a positive,
// -0, NaN, -Infinity, a fraction -- is ERR_OUT_OF_RANGE (RangeError). A runtime
// that skips validation returns "Unknown system error 0" (or undefined) where
// Node throws, so libuv-errno plumbing silently swallows a caller's bug.
import util from "node:util";

// [label, value] pairs -- the label is written out rather than derived so -0
// and {} stay distinguishable in the transcript.
const inputs = [
  ["string", "x"],
  ["object", {}],
  ["undefined", undefined],
  ["null", null],
  ["bigint", 1n],
  ["boolean", true],
  ["zero", 0],
  ["positive", 5],
  ["negative-zero", -0],
  ["NaN", NaN],
  ["-Infinity", -Infinity],
  ["fraction", -2.5],
];

for (const fnName of ["getSystemErrorName", "getSystemErrorMessage"]) {
  const fn = util[fnName];
  for (const [label, value] of inputs) {
    let out;
    try {
      out = `NO-THROW ${JSON.stringify(fn(value))}`;
    } catch (e) {
      out = `${e.constructor.name}|${e.name}|${e.code}|${e.message}`;
    }
    console.log(`${fnName} ${label} -> ${out}`);
  }
}

// A negative integer typed as a float value (-2.0) is still an integer and must
// go through to the table lookup, not the range check.
console.log("integral-float", JSON.stringify(util.getSystemErrorName(-2.0)));
