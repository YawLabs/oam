// assert.CallTracker (deprecated DEP0173): report().operator, getCalls()
// {thisArg, arguments} records, verify() AssertionError message, and the
// ERR_INVALID_ARG_VALUE on an untracked function. Deterministic stable fields
// only (report().stack is an Error, omitted).
import assert from "node:assert";

const tracker = new assert.CallTracker();
function foo() {}
const f = tracker.calls(foo, 2);

console.log("report-before", JSON.stringify(tracker.report().map((e) => ({ op: e.operator, exp: e.expected, act: e.actual }))));
f(1);
f(2);
console.log("report-satisfied-len", tracker.report().length);
console.log("getCalls", JSON.stringify(tracker.getCalls(f)));

f(3);
try {
  tracker.verify();
} catch (e) {
  console.log("verify", e.code, e.message);
}

try {
  tracker.getCalls(() => {});
} catch (e) {
  console.log("untracked", e.code);
}
