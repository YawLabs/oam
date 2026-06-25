// process fires a synchronous 'exit' event at natural termination with
// process._exiting set (Node parity); and oam's web globals are non-enumerable
// like Node's, so for..in / Object.keys(globalThis) exclude them.
import process from "node:process";

process.on("exit", (code) => {
  console.log("exit-event", code, process._exiting);
});

for (const name of ["Buffer", "URL", "Headers", "TextEncoder", "process"]) {
  const d = Object.getOwnPropertyDescriptor(globalThis, name);
  console.log(name, "enumerable=" + (d ? d.enumerable : "absent"));
}
console.log("keys-has-Buffer", Object.keys(globalThis).includes("Buffer"));
console.log("body-done");
