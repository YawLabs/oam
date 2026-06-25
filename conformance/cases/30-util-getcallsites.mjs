// util.getCallSites (Node >=22.9): structured current call stack. Used by
// Node's own test/common (mustNotCall), so it gates a chunk of the node-suite.
// Assert only the platform-stable shape (scriptId / absolute paths vary).
import util from "node:util";

function inner() {
  return util.getCallSites();
}
const sites = inner();

console.log("isArray", Array.isArray(sites));
console.log("keys", Object.keys(sites[0]).sort().join(","));
console.log("callerFn", sites[0].functionName); // [0] == the caller of getCallSites
console.log(
  "types",
  typeof sites[0].lineNumber,
  typeof sites[0].columnNumber,
  typeof sites[0].scriptName,
);
console.log("endsWith", sites[0].scriptName.endsWith("30-util-getcallsites.mjs"));
console.log("colMirror", sites[0].column === sites[0].columnNumber);
