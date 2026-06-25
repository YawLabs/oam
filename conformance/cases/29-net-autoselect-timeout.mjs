// net.get/setDefaultAutoSelectFamilyAttemptTimeout (Happy-Eyeballs default
// attempt timeout). These are the APIs Node's test/common requires at load,
// so they gate the entire vendored Node-suite harness; guard them here so
// the behaviour can never silently regress.
import net from "node:net";

console.log("default", net.getDefaultAutoSelectFamilyAttemptTimeout());
net.setDefaultAutoSelectFamilyAttemptTimeout(500);
console.log("after-set", net.getDefaultAutoSelectFamilyAttemptTimeout());
net.setDefaultAutoSelectFamilyAttemptTimeout(3); // clamps up to the 10ms floor
console.log("clamped-to-min", net.getDefaultAutoSelectFamilyAttemptTimeout());
try {
  net.setDefaultAutoSelectFamilyAttemptTimeout(0);
  console.log("no-throw");
} catch (e) {
  console.log("threw-rangeerror", e instanceof RangeError);
}
