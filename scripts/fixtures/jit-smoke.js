// JIT smoke: run against the SIGNED macOS release binaries by
// scripts/build-remote.sh (jit_smoke), on both arches -- x86_64 under Rosetta.
//
// A hardened-runtime signature without the right entitlements does not stop
// oam from starting: `oam --version` and the plain `ci smoke` print fine. It
// kills the process the first time V8 needs executable memory -- an optimizing
// tier-up, a RegExp compiled to native code, a WebAssembly module -- with a
// SIGTRAP or "Ran out of executable memory". run_test and run_conformance only
// ever execute the UNSIGNED build, so without this nothing on the release path
// would make the signed bytes JIT anything before they ship.
//
// Each workload is checked against a value computed here, so a wrong answer
// fails too, not only a crash. The only stdout on success is the last line,
// which the gate compares byte for byte. Plain script, no imports: it must run
// from any directory.
"use strict";

function check(name, got, want) {
  if (got !== want) {
    throw new Error(`jit smoke: ${name} returned ${got}, expected ${want}`);
  }
}

// 1. A hot loop, long enough to tier up through the optimizing compilers.
function mix(x, i) {
  return (Math.imul(x, 31) + i * 7 + (i >>> 3)) | 0;
}
let acc = 1;
for (let i = 0; i < 3000000; i++) acc = mix(acc, i);
// The same recurrence written inline (a different function, so different
// compiled code): the two must agree.
let cold = 1;
for (let i = 0; i < 3000000; i++) cold = (Math.imul(cold, 31) + i * 7 + (i >>> 3)) | 0;
check("hot loop", acc, cold);

// 2. RegExp: irregexp tiers a hot pattern up from bytecode to native code.
const re = /(\d{3})-(\d{4})/g;
let text = "";
for (let i = 0; i < 2000; i++) text += `call ${100 + (i % 900)}-${1000 + i} or not${i} `;
let matches = 0;
let digits = 0;
for (let round = 0; round < 25; round++) {
  re.lastIndex = 0;
  let m;
  while ((m = re.exec(text)) !== null) {
    matches++;
    digits += m[1].length + m[2].length;
  }
}
check("regexp matches", matches, 25 * 2000);
check("regexp digits", digits, 25 * 2000 * 7);

// 3. WebAssembly: compiling a module at all needs executable memory, and the
// hot call loop tiers it up from Liftoff to TurboFan.
//   (module (func (export "add") (param i32 i32) (result i32)
//     local.get 0 local.get 1 i32.add))
const bytes = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f,
  0x03, 0x02, 0x01, 0x00,
  0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00,
  0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b,
]);
const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {});
const add = instance.exports.add;
let wsum = 0;
for (let i = 0; i < 1000000; i++) wsum = add(wsum, i & 0xff);
let jsum = 0;
for (let i = 0; i < 1000000; i++) jsum = (jsum + (i & 0xff)) | 0;
check("wasm add", wsum, jsum);

console.log("jit smoke ok");
