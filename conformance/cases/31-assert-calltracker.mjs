// assert.CallTracker (DEP0173) is End-of-Life in node v26: the class is gone
// from node:assert and node:assert/strict, as an own property, a named ESM
// export and through the strict namespace alike, and `new` on the missing
// member is a plain TypeError.
//
// Edited for the v26.13.1 parity target: the case used to probe the class's
// report()/getCalls()/verify() behavior, which node 26 can no longer run (the
// first `new assert.CallTracker()` throws). It now pins the removal itself.
import assert from "node:assert";
import * as ns from "node:assert";
import strict from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
console.log("typeof", typeof assert.CallTracker, typeof strict.CallTracker, typeof assert.strict.CallTracker);
console.log("in", "CallTracker" in assert, "CallTracker" in strict, "CallTracker" in ns);
console.log("required", typeof require("assert").CallTracker, typeof require("node:assert/strict").CallTracker);
try {
  new assert.CallTracker();
} catch (e) {
  console.log("new", e.name, e.message);
}
console.log("keys", Object.keys(assert).filter((k) => /tracker/i.test(k)).length);
