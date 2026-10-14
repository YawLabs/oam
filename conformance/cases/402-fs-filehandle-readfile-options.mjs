// FileHandle.readFile checks its options before it reads (#351). node runs
// the shared options check first (an unknown encoding is
// `ERR_INVALID_ARG_VALUE`, a non-object is `ERR_INVALID_ARG_TYPE`), then
// builds the decoder for the encoding -- which refuses `"buffer"`,
// `ERR_UNKNOWN_ENCODING` -- then looks at the signal, stats the handle,
// looks at the signal again, and only then reads: up to the size the file
// had at that stat, so what is appended meanwhile is left for the next read.
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
// Aborted right after the call, before the first read: nothing is read, so
// the whole file is still there for the read that follows.
for (const [label, target, flag] of [["file", big, "r"], ["empty file", empty, "r"], ["write-only handle", file, "a"]]) {
  for (const when of ["same tick", "next tick", "microtask"]) {
    const handle = await fs.promises.open(target, flag);
    const controller = new AbortController();
    const reading = answer(handle.readFile({ signal: controller.signal }));
    if (when === "same tick") controller.abort();
    else if (when === "next tick") process.nextTick(() => controller.abort());
    else queueMicrotask(() => controller.abort());
    console.log(`aborted, ${when} | ${label} | ${await reading} | then ${await answer(handle.readFile())}`);
    await handle.close();
  }
}
// What is appended while a read is under way: the read goes up to the size
// the file had when its fstat ran, and the next one takes the rest. Which
// side of that fstat a same-tick append lands on is a race between the main
// thread and the pool, in node as here (node split it the other way 1 time
// in 25 under load), so what is printed is what holds either way: the two
// reads between them give the whole file once, in order.
{
  const growing = path.join(made, "growing");
  fs.writeFileSync(growing, "0123456789");
  const handle = await fs.promises.open(growing, "r");
  const reading = answer(handle.readFile("utf8"));
  fs.appendFileSync(growing, "ABCDE");
  const first = await reading;
  const second = await answer(handle.readFile("utf8"));
  const split = [first, second].join(" + ");
  const whole = ['string "0123456789" + string "ABCDE"', 'string "0123456789ABCDE" + string ""'];
  console.log("appended in the same tick | " + (whole.includes(split) ? "the whole file, once" : split));
  await handle.close();
  // An empty file has no size to read up to: it is read to its end.
  const grown = path.join(made, "grown");
  fs.writeFileSync(grown, "");
  const other = await fs.promises.open(grown, "r");
  const second = answer(other.readFile("utf8"));
  fs.appendFileSync(grown, "XYZ");
  console.log("an empty file, appended to | " + (await second));
  await other.close();
}
// A string is the only thing that names an encoding.
for (const encoding of [["utf8"], new String("utf8"), { toString: () => "hex" }]) {
  const handle = await fs.promises.open(file, "r");
  console.log(`encoding ${Object.prototype.toString.call(encoding)} | ${await answer(handle.readFile({ encoding }))} | isEncoding ${Buffer.isEncoding(encoding)}`);
  await handle.close();
}
// fs.promises.readFile of a handle that is closed: the encoding and the
// signal are heard first there too, and then the descriptor is -1.
{
  const handle = await fs.promises.open(file, "r");
  await handle.close();
  for (const options of [undefined, "buffer", "nope", { signal: AbortSignal.abort("x") }]) {
    const name = options === undefined ? "undefined" : typeof options === "object" ? "an aborted signal" : JSON.stringify(options);
    console.log(`fs.promises.readFile(closed handle) | ${name} | ${await answer(fs.promises.readFile(handle, options))}`);
  }
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
