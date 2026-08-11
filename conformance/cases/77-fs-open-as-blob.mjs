// fs.openAsBlob -- the last easy one of the remaining fs export gaps, because
// oam already had a real web Blob (arrayBuffer/text/stream/slice/bytes).
//
// Three shapes here are counter-intuitive and were measured, not inferred:
//
//   1. options.type is coerced with `|| ''` BEFORE the string check, so
//      {type: 0 | false | null | NaN} all yield "" and only a TRUTHY
//      non-string throws. node's own source: `const type = options.type || '';
//      validateString(type, 'options.type')`.
//   2. An unreadable path does NOT surface the fs error. node throws a
//      TypeError with code ERR_INVALID_ARG_VALUE and the message "Unable to
//      open file as blob" -- no errno, no syscall, no path. Leaking ENOENT
//      here would be a divergence, not a courtesy.
//   3. node returns an instance of an internal TransferableBlob subclass with
//      an own `constructor` planted back to Blob, and the result is NOT
//      structuredClone-able. A plain Blob still is.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const file = path.join(os.tmpdir(), `oam-conf-oab-${process.pid}.txt`);
fs.writeFileSync(file, "hello world");

const blob = await fs.openAsBlob(file);
console.log("size:", blob.size, "| type:", JSON.stringify(blob.type));
console.log("ctor name:", blob.constructor.name, "| instanceof Blob:", blob instanceof Blob);
// Enumerable own property, so a structural comparison sees it.
console.log("own keys:", JSON.stringify(Object.keys(blob)));
console.log("text:", await blob.text());
console.log("arrayBuffer bytes:", new Uint8Array(await blob.arrayBuffer()).length);

const part = blob.slice(0, 5);
console.log("slice:", await part.text(), "| slice ctor:", part.constructor.name);

const typed = await fs.openAsBlob(file, { type: "text/plain" });
console.log("explicit type:", JSON.stringify(typed.type));

// Falsy types coerce to "", truthy non-strings throw.
for (const [label, v] of [["0", 0], ["false", false], ["null", null], ["NaN", NaN], ["undefined", undefined]]) {
  const b = await fs.openAsBlob(file, { type: v });
  console.log(`type=${label} ->`, JSON.stringify(b.type));
}
try {
  await fs.openAsBlob(file, { type: 123 });
  console.log("type=123 -> no throw");
} catch (e) {
  console.log("type=123 ->", e.code);
}

// An unreadable path: the fs error is deliberately not surfaced.
try {
  await fs.openAsBlob(`${file}.missing`);
  console.log("missing -> no throw");
} catch (e) {
  console.log("missing ->", e.name, e.code, JSON.stringify(e.message));
  console.log("missing own keys:", JSON.stringify(Object.keys(e)));
}

// File-backed blobs are not cloneable -- at the top level or nested. A plain
// Blob is unaffected, so the check must not over-reach.
for (const [label, make] of [
  ["top-level", () => blob],
  ["nested", () => ({ deep: { blob } })],
  ["plain Blob", () => new Blob(["plain"])],
]) {
  try {
    structuredClone(make());
    console.log(`structuredClone ${label}: ok`);
  } catch (e) {
    console.log(`structuredClone ${label}:`, e.name, "|", e.message);
  }
}

// A Blob must not expose its internals. oam stored them as own ENUMERABLE
// properties, so Object.keys(blob) was ["_bytes","_type"] and -- far worse --
// JSON.stringify(blob) serialised the WHOLE payload as a numeric object where
// node gives {}. Any log line or API response carrying an object with a Blob
// in it dumped the entire buffer.
const plain = new Blob(["hi"], { type: "text/plain" });
console.log("plain Blob own keys:", JSON.stringify(Object.keys(plain)));
console.log("plain Blob stringify:", JSON.stringify(plain));
console.log("plain Blob spread:", JSON.stringify({ ...plain }));
// ...while still behaving like a Blob.
console.log("plain Blob size/type:", plain.size, JSON.stringify(plain.type));
console.log("plain Blob text:", await plain.text());

fs.unlinkSync(file);
