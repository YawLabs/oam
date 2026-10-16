// Buffer#toString's receiver dispatch on the typed-array brands ADJACENT to
// Uint8Array, and on a Uint8Array subclass.
// - The dispatch is `receiver instanceof Uint8Array`; every other view --
//   Uint8ClampedArray, Int8Array, Float64Array, BigInt64Array -- is NOT one,
//   so it falls to the TypeError branch (the missing `<enc>Slice`), exactly as
//   node. A clamped or int8 view looks byte-compatible but is not a
//   Uint8Array, which is the trap 457's u8/u16/DataView/object set does not
//   cover.
// - A Uint8Array subclass that does not override `utf8Slice` IS a Uint8Array,
//   so it dispatches through the inherited method and decodes -- except over an
//   empty range with an encoding given, which is "" before any lookup.
// Measured on node v22.22.2.
import { Buffer } from "node:buffer";

const show = (label, f) => {
  try {
    console.log(label, JSON.stringify(f()));
  } catch (e) {
    console.log(label, "THROW", e.name, e.code ? e.code : "");
  }
};

class Sub extends Uint8Array {}

const receivers = [
  ["u8c-5", () => new Uint8ClampedArray([104, 105, 106, 107, 108])],
  ["i8-5", () => new Int8Array([104, 105, 106, 107, 108])],
  ["f64-2", () => new Float64Array([1, 2])],
  ["bi64-2", () => new BigInt64Array([1n, 2n])],
  ["sub-5", () => new Sub([104, 105, 106, 107, 108])],
  ["sub-0", () => new Sub(0)],
];
const argsets = [[], ["utf8"], ["utf8", 0, 1], ["hex"], ["utf8", 0, 0]];
for (const [name, make] of receivers) {
  for (const args of argsets) {
    show(`${name} ${JSON.stringify(args)}`, () => Buffer.prototype.toString.apply(make(), args));
  }
}
