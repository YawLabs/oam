// FileHandle.readFile checks its options before it reads (#351). node runs
// the shared options check first (an unknown encoding is
// `ERR_INVALID_ARG_VALUE`, a non-object is `ERR_INVALID_ARG_TYPE`), then
// builds the decoder for the encoding -- which refuses `"buffer"`,
// `ERR_UNKNOWN_ENCODING` -- then looks at the signal, and only then reads.
// So what is wrong with the call is what is reported, whatever the handle
// holds: for an empty file, and for a handle that cannot be read at all.
//
// Regression guard: oam read first and decoded at the end. An unknown
// encoding was `ERR_UNKNOWN_ENCODING` (the wrong code), was not reported at
// all for an empty file (`""`), and on a handle that cannot be read the
// read's `EBADF` came back instead. The signal was ignored.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const made = fs.mkdtempSync(path.join(os.tmpdir(), "oam-case-402-"));
// Whichever row throws, the directory goes.
process.on("exit", () => {
  try {
    fs.rmSync(made, { recursive: true, force: true });
  } catch {}
});
const file = path.join(made, "f");
fs.writeFileSync(file, "0123456789");
const empty = path.join(made, "empty");
fs.writeFileSync(empty, "");
const big = path.join(made, "big");
fs.writeFileSync(big, Buffer.alloc(300000, 0x61));

const value = (v) => (typeof v === "string" ? "string " + JSON.stringify(v.length > 20 ? v.length : v) : `${v.constructor.name} ${v.length}`);
const shape = (e) =>
  e && e.name === "AbortError"
    ? ["AbortError", e.code, String(e.cause)].join(" / ")
    : [e.constructor.name, e.code, e.syscall ?? "-", e.message].join(" / ");
const answer = (p) => p.then(value, shape);

const targets = [
  ["file", file, "r"],
  ["empty file", empty, "r"],
  ["write-only handle", file, "a"],
];
const optionSets = [
  undefined, null, "utf8", "hex", "latin1", "base64", "buffer", "nope", "",
  { encoding: null }, { encoding: "utf8" }, { encoding: "buffer" }, { encoding: "nope" }, { encoding: 5 },
  5, true, {}, { flag: "zz" }, { signal: 1 }, { signal: null },
];
for (const [label, target, flag] of targets) {
  for (const options of optionSets) {
    const handle = await fs.promises.open(target, flag);
    const name = options === undefined ? "undefined" : JSON.stringify(options);
    console.log(`${label} | ${name} | handle.readFile: ${await answer(handle.readFile(options))}`);
    await handle.close();
    const other = await fs.promises.open(target, flag);
    console.log(`${label} | ${name} | fs.promises.readFile(handle): ${await answer(fs.promises.readFile(other, options))}`);
    await other.close();
  }
}

// A signal: already aborted, and aborted while the read is under way.
for (const encoding of [undefined, "utf8"]) {
  const handle = await fs.promises.open(big, "r");
  console.log(`aborted | ${encoding} | ${await answer(handle.readFile({ signal: AbortSignal.abort(), encoding }))}`);
  console.log(`aborted with a reason | ${encoding} | ${await answer(handle.readFile({ signal: AbortSignal.abort("why"), encoding }))}`);
  // The descriptor is where it was: nothing was read.
  console.log(`after the aborts | ${encoding} | ${await answer(handle.readFile({ encoding }))}`);
  await handle.close();
}
{
  const handle = await fs.promises.open(big, "r");
  const controller = new AbortController();
  const reading = answer(handle.readFile({ signal: controller.signal }));
  controller.abort();
  console.log("aborted in the same tick | " + (await reading));
  await handle.close();
}
// The closed-handle check comes before all of it.
{
  const handle = await fs.promises.open(file, "r");
  await handle.close();
  for (const options of [undefined, "nope", "buffer", 5, { signal: AbortSignal.abort() }]) {
    const name = options === undefined ? "undefined" : typeof options === "object" ? "an aborted signal" : JSON.stringify(options);
    console.log(`closed handle | ${name} | ${await answer(handle.readFile(options))}`);
  }
}
// Reading from where the descriptor is, and twice.
{
  const handle = await fs.promises.open(file, "r");
  await handle.read(Buffer.alloc(4), 0, 4, null);
  console.log("after a read of 4 | " + (await answer(handle.readFile("utf8"))) + " | again " + (await answer(handle.readFile("utf8"))));
  await handle.close();
}
